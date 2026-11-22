"""The R2 client: AWS's published SigV4 examples, and requests against a fake of R2's S3 API."""
import datetime
import hashlib
import http.server
import re
import tempfile
import threading
import unittest
import urllib.parse
from pathlib import Path
import storage

KEY_ID, SECRET = 'AKIAIOSFODNN7EXAMPLE', 'wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY'
WHEN = datetime.datetime(2013, 5, 24, tzinfo=datetime.timezone.utc)


class Signatures(unittest.TestCase):
    """AWS's examples, from "Signature Calculations for the Authorization Header" in the Amazon S3 API reference."""

    def test_get_object(self):
        signed = storage.authorization('GET', 'examplebucket.s3.amazonaws.com', '/test.txt', {}, {'Range': 'bytes=0-9'},
                                       storage.EMPTY_SHA256, KEY_ID, SECRET, WHEN, region='us-east-1')
        self.assertEqual(signed['Authorization'],
                         'AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, '
                         'SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, '
                         'Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41')
        self.assertEqual(signed['x-amz-date'], '20130524T000000Z')

    def test_put_object(self):
        # The key, test$file.text, has a character the canonical URI escapes.
        body = b'Welcome to Amazon S3.'
        signed = storage.authorization('PUT', 'examplebucket.s3.amazonaws.com', '/test$file.text', {},
                                       {'Date': 'Fri, 24 May 2013 00:00:00 GMT', 'x-amz-storage-class': 'REDUCED_REDUNDANCY'},
                                       hashlib.sha256(body).hexdigest(), KEY_ID, SECRET, WHEN, region='us-east-1')
        self.assertTrue(signed['Authorization'].endswith(
            'Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd'), signed['Authorization'])


class FakeR2:
    """What the client uses of R2's S3 API, path-style, in memory: GET, HEAD and PUT, each checked against its
    signature and its payload's SHA-256, and If-Match and If-None-Match evaluated atomically."""

    def __init__(self, bucket='lanescape-browser', key_id='id', secret='secret'):
        self.bucket, self.key_id, self.secret = bucket, key_id, secret
        self.objects, self.lock, self.refuse, self.fail_next, self.requests = {}, threading.Lock(), None, 0, []
        self.truncate_next = 0
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *args):
                pass

            def answer(self, status, headers=None, body=b''):
                self.send_response(status)
                for k, v in (headers or {}).items():
                    self.send_header(k, v)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                if self.command != 'HEAD':
                    if fake.truncate_next and status == 200 and body:
                        # The connection drops partway through the body.
                        fake.truncate_next -= 1
                        self.wfile.write(body[:len(body) // 2])
                        self.wfile.flush()
                        self.close_connection = True
                        return
                    self.wfile.write(body)

            def error(self, status, code):
                self.answer(status, {'Content-Type': 'application/xml'}, f'<Error><Code>{code}</Code></Error>'.encode())

            def handle_one(self):
                fake.requests.append(self.command)
                length = int(self.headers.get('Content-Length') or 0)
                body = self.rfile.read(length) if length else b''
                if fake.fail_next:
                    fake.fail_next -= 1
                    return self.error(503, 'ServiceUnavailable')
                if fake.refuse:
                    return self.error(fake.refuse, 'AccessDenied')
                if not fake.verify(self, body):
                    return self.error(403, 'SignatureDoesNotMatch')
                key = urllib.parse.unquote(urllib.parse.urlsplit(self.path).path).removeprefix('/')
                with fake.lock:
                    found = fake.objects.get(key)
                    if self.command in ('GET', 'HEAD'):
                        if found is None:
                            return self.error(404, 'NoSuchKey')
                        return self.answer(200, {'ETag': found['etag'], **found['headers']}, found['body'])
                    match, none = self.headers.get('If-Match'), self.headers.get('If-None-Match')
                    if (match and (found is None or match not in ('*', found['etag']))) or \
                            (none and found is not None and none in ('*', found['etag'])):
                        return self.error(412, 'PreconditionFailed')
                    etag = f'"{hashlib.md5(body).hexdigest()}"'
                    kept = {k.lower(): v for k, v in self.headers.items()
                            if k.lower() in ('content-type', 'cache-control') or k.lower().startswith('x-amz-meta-')}
                    fake.objects[key] = {'body': body, 'etag': etag, 'headers': kept}
                return self.answer(200, {'ETag': etag})

            do_GET = do_HEAD = do_PUT = handle_one

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def verify(self, request, body):
        """The signature, recomputed from what was sent, and the payload's SHA-256."""
        auth = request.headers.get('Authorization', '')
        match = re.fullmatch(r'AWS4-HMAC-SHA256 Credential=([^/]+)/(\d{8})/auto/s3/aws4_request, '
                             r'SignedHeaders=([a-z0-9;-]+), Signature=[0-9a-f]{64}', auth)
        if not match or match[1] != self.key_id:
            return False
        payload = request.headers.get('x-amz-content-sha256')
        if request.command == 'PUT' and payload != hashlib.sha256(body).hexdigest():
            return False
        names = match[3].split(';')
        headers = {n: request.headers.get(n) for n in names if n not in ('host', 'x-amz-date', 'x-amz-content-sha256')}
        if None in headers.values():
            return False
        when = datetime.datetime.strptime(request.headers['x-amz-date'], '%Y%m%dT%H%M%SZ').replace(tzinfo=datetime.timezone.utc)
        path = urllib.parse.unquote(urllib.parse.urlsplit(request.path).path)
        expected = storage.authorization(request.command, request.headers['Host'], path, {}, headers, payload,
                                         self.key_id, self.secret, when)
        return expected['Authorization'] == auth

    def client(self, **kw):
        return storage.R2('0123', self.bucket, self.key_id, self.secret,
                          endpoint=f'http://127.0.0.1:{self.server.server_address[1]}', sleep=lambda s: None, **kw)

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


class Client(unittest.TestCase):
    def setUp(self):
        self.fake = FakeR2()
        self.addCleanup(self.fake.stop)
        self.r2 = self.fake.client()

    def test_put_get_head_digest(self):
        etag = self.r2.put('v1/a.json', b'{}', 'application/json', storage.IMMUTABLE)
        stored = self.fake.objects['lanescape-browser/v1/a.json']
        self.assertEqual(stored['etag'], etag)
        self.assertEqual(stored['headers'], {'content-type': 'application/json', 'cache-control': storage.IMMUTABLE,
                                             'x-amz-meta-sha256': hashlib.sha256(b'{}').hexdigest()})
        self.assertEqual(self.r2.get('v1/a.json'), (b'{}', etag))
        self.assertEqual(self.r2.head('v1/a.json')['sha256'], hashlib.sha256(b'{}').hexdigest())
        self.assertEqual(self.r2.digest('v1/a.json'), (2, hashlib.sha256(b'{}').hexdigest()))
        self.assertIsNone(self.r2.get('v1/none'))
        self.assertIsNone(self.r2.head('v1/none'))
        self.assertIsNone(self.r2.digest('v1/none'))

    def test_conditional_writes(self):
        first = self.r2.put('v1/latest', b'1\n', 'text/plain', storage.NO_STORE, if_none_match=True)
        with self.assertRaises(storage.PreconditionFailed):
            self.r2.put('v1/latest', b'2\n', 'text/plain', storage.NO_STORE, if_none_match=True)
        second = self.r2.put('v1/latest', b'2\n', 'text/plain', storage.NO_STORE, if_match=first)
        with self.assertRaises(storage.PreconditionFailed):
            self.r2.put('v1/latest', b'3\n', 'text/plain', storage.NO_STORE, if_match=first)
        self.assertEqual(self.r2.get('v1/latest'), (b'2\n', second))

    def test_files_stream_and_are_immutable(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'package.tar.gz'
            path.write_bytes(b'package' * 100000)
            expected = storage.identity(path)
            self.assertEqual(storage.put_immutable(self.r2, path, 'v1/1/p.tar.gz', 'application/gzip'), expected)
            # A retry of the same file finds it and accepts it.
            self.assertEqual(storage.put_immutable(self.r2, path, 'v1/1/p.tar.gz', 'application/gzip'), expected)
            path.write_bytes(b'other')
            with self.assertRaisesRegex(RuntimeError, 'immutable object conflict'):
                storage.put_immutable(self.r2, path, 'v1/1/p.tar.gz', 'application/gzip')
        self.assertEqual(self.r2.digest('v1/1/p.tar.gz'), (expected['bytes'], expected['sha256']))

    def test_retries_and_refusals(self):
        self.fake.fail_next = 2
        self.assertIsNone(self.r2.get('v1/none'))
        self.fake.fail_next = 3
        with self.assertRaisesRegex(storage.R2Error, '503 ServiceUnavailable'):
            self.r2.get('v1/none')
        self.fake.fail_next = 0
        self.fake.refuse = 403
        with self.assertRaisesRegex(storage.R2Error, 'GET v1/none with 403 AccessDenied'):
            self.r2.get('v1/none')
        # A HEAD's refusal has no body to name it.
        with self.assertRaisesRegex(storage.R2Error, 'HEAD v1/none with 403'):
            self.r2.head('v1/none')
        # A wrong secret is refused, not taken for a missing object.
        self.fake.refuse = None
        wrong = storage.R2('0123', 'lanescape-browser', 'id', 'not-the-secret',
                           endpoint=f'http://127.0.0.1:{self.fake.server.server_address[1]}', sleep=lambda s: None)
        with self.assertRaisesRegex(storage.R2Error, 'SignatureDoesNotMatch'):
            wrong.get('v1/none')

    def test_a_download_dropped_partway_is_read_again(self):
        self.r2.put('v1/p', b'x' * 100000, 'application/gzip', storage.IMMUTABLE)
        self.fake.truncate_next = 1
        self.assertEqual(self.r2.digest('v1/p'), (100000, hashlib.sha256(b'x' * 100000).hexdigest()))
        self.fake.truncate_next = 1
        self.assertEqual(self.r2.get('v1/p')[0], b'x' * 100000)
        self.fake.truncate_next = storage.ATTEMPTS
        with self.assertRaises(Exception):
            self.r2.digest('v1/p')

    def test_the_host_signed_is_the_host_sent(self):
        # An endpoint with its scheme's default port signs and sends the host alone.
        r2 = storage.R2('0123', 'b', 'id', 'secret', endpoint='https://example.invalid:443')
        self.assertEqual((r2.host, r2.port), ('example.invalid', None))
        r2 = storage.R2('0123', 'b', 'id', 'secret', endpoint='http://127.0.0.1:8080')
        self.assertEqual((r2.host, r2.port), ('127.0.0.1:8080', 8080))

    def test_malformed_credentials_are_refused_without_their_values(self):
        for key_id, secret in (('id\nX-Injected: 1', 'secret'), ('id', 'sec ret'), ('', 'secret'), ('id', '')):
            with self.subTest(key_id=key_id, secret=secret):
                with self.assertRaises(ValueError) as raised:
                    storage.R2('0123', 'b', key_id, secret)
                self.assertNotIn('Injected', str(raised.exception))
                self.assertNotIn('sec ret', str(raised.exception))

    def test_keys_and_configuration(self):
        for key in ('', '/v1', 'v1//x', 'v1/../x', 'v1/./x'):
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.r2.get(key)
        with self.assertRaisesRegex(SystemExit, 'R2_SECRET_ACCESS_KEY'):
            storage.R2.from_environment(env={'R2_ACCESS_KEY_ID': 'id'})
        r2 = storage.R2.from_environment(env={'R2_ACCESS_KEY_ID': 'id', 'R2_SECRET_ACCESS_KEY': 'secret'})
        self.assertEqual((r2.scheme, r2.host, r2.bucket),
                         ('https', '6d39a82297745b856f8c09e7e0b91b3a.r2.cloudflarestorage.com', 'lanescape-browser'))


if __name__ == '__main__':
    unittest.main()
