"""Cloudflare R2, through its S3 API, with the standard library alone (specs/0004-releases.md, Publishing).

Requests are signed with AWS Signature Version 4 in an Authorization header, with R2's region, auto, and the
payload's SHA-256, which R2 checks. Writes can be conditional: If-None-Match: * creates an object only if there is
none, and If-Match: <ETag> replaces one only if it is still what was read; R2 evaluates both atomically and answers
412 when one fails. Credentials and signatures are never printed.
"""
import datetime
import hashlib
import hmac
import http.client
import json
import os
import re
import time
import urllib.parse
from pathlib import Path

REGION = 'auto'
EMPTY_SHA256 = hashlib.sha256(b'').hexdigest()
# Packages and manifests never change once written; vN/latest does, and is never cached.
IMMUTABLE = 'public, max-age=31536000, immutable'
NO_STORE = 'no-store'
# What CD reads: the bucket and its account, which aren't secret. The keys come from the environment.
CONFIG = Path(__file__).resolve().parent.parent/'.github'/'release.json'
ATTEMPTS = 3


class R2Error(RuntimeError):
    """An answer R2 shouldn't have given: a refusal, a server error, or one the caller doesn't expect."""

    def __init__(self, method, key, status, code):
        super().__init__(f'R2 answered {method} {key} with {status} {code}')
        self.status, self.code = status, code


class PreconditionFailed(Exception):
    """A conditional write's condition didn't hold: the object exists, or isn't what was read."""


def identity(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return {'bytes': Path(path).stat().st_size, 'sha256': digest.hexdigest()}


def _hmac(key, text):
    return hmac.new(key, text.encode(), hashlib.sha256).digest()


def _encode(text, safe):
    # SigV4's UriEncode: every byte but A-Z a-z 0-9 - . _ ~ as %XX, in capitals.
    return urllib.parse.quote(text, safe=safe)


def authorization(method, host, path, query, headers, payload_sha256, access_key_id, secret_access_key, when,
                  region=REGION):
    """The headers that sign a request: x-amz-date, x-amz-content-sha256 and Authorization, over host and every
    header given. path is the object's path, unescaped; query maps names to values."""
    amz_date = when.strftime('%Y%m%dT%H%M%SZ')
    signed = {k.lower(): ' '.join(str(v).split()) for k, v in headers.items()}
    signed.update({'host': host, 'x-amz-content-sha256': payload_sha256, 'x-amz-date': amz_date})
    names = sorted(signed)
    canonical = '\n'.join([
        method,
        _encode(path, '/~'),
        '&'.join(sorted(f'{_encode(k, "~")}={_encode(v, "~")}' for k, v in query.items())),
        ''.join(f'{name}:{signed[name]}\n' for name in names),
        ';'.join(names),
        payload_sha256,
    ])
    scope = f'{amz_date[:8]}/{region}/s3/aws4_request'
    to_sign = '\n'.join(['AWS4-HMAC-SHA256', amz_date, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    key = ('AWS4' + secret_access_key).encode()
    for part in scope.split('/'):
        key = _hmac(key, part)
    signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    return {
        'x-amz-date': amz_date,
        'x-amz-content-sha256': payload_sha256,
        'Authorization': f'AWS4-HMAC-SHA256 Credential={access_key_id}/{scope}, SignedHeaders={";".join(names)}, '
                         f'Signature={signature}',
    }


def _error_code(body):
    match = re.search(rb'<Code>([A-Za-z]{1,64})</Code>', body or b'')
    return match[1].decode() if match else 'no error code'


class R2:
    """One bucket of an R2 account."""

    def __init__(self, account_id, bucket, access_key_id, secret_access_key, endpoint=None, clock=None, sleep=time.sleep):
        # Checked before anything is signed or sent: a malformed value would otherwise reach http.client, whose
        # error quotes the header it refused. Neither message holds a value.
        if not re.fullmatch(r'[A-Za-z0-9]{1,128}', access_key_id or ''):
            raise ValueError('the R2 access key ID must be letters and digits')
        if not re.fullmatch(r'[!-~]{1,256}', secret_access_key or ''):
            raise ValueError('the R2 secret access key must be printable characters without spaces')
        url = urllib.parse.urlsplit(endpoint or f'https://{account_id}.r2.cloudflarestorage.com')
        if url.scheme not in ('https', 'http') or not url.hostname or url.path not in ('', '/') or url.query or \
                url.username:
            raise ValueError('an R2 endpoint is a scheme and a host')
        # The Host header sent is exactly the one signed, without a port that is the scheme's default.
        port = url.port if url.port not in (None, {'https': 443, 'http': 80}[url.scheme]) else None
        self.scheme, self.hostname, self.port, self.bucket = url.scheme, url.hostname, port, bucket
        self.host = url.hostname if port is None else f'{url.hostname}:{port}'
        self.access_key_id, self.secret_access_key = access_key_id, secret_access_key
        self.clock = clock or (lambda: datetime.datetime.now(datetime.timezone.utc))
        self.sleep = sleep

    @classmethod
    def from_environment(cls, env=os.environ, config=CONFIG):
        """The release bucket of .github/release.json, signed for with R2_ACCESS_KEY_ID and R2_SECRET_ACCESS_KEY.
        LANESCAPE_BROWSER_R2_ENDPOINT replaces the account's endpoint, for tests only."""
        settings = json.loads(Path(config).read_text())['r2']
        missing = [name for name in ('R2_ACCESS_KEY_ID', 'R2_SECRET_ACCESS_KEY') if not env.get(name)]
        if missing:
            raise SystemExit(f'{" and ".join(missing)} must be set: the publish and promote environments hold them')
        return cls(settings['account_id'], settings['bucket'], env['R2_ACCESS_KEY_ID'], env['R2_SECRET_ACCESS_KEY'],
                   endpoint=env.get('LANESCAPE_BROWSER_R2_ENDPOINT'))

    def _path(self, key):
        if not key or key.startswith('/') or any(part in ('', '.', '..') for part in key.split('/')):
            raise ValueError(f'not an object key: {key!r}')
        return f'/{self.bucket}/{key}'

    def _connection(self):
        cls = http.client.HTTPSConnection if self.scheme == 'https' else http.client.HTTPConnection
        return cls(self.hostname, self.port, timeout=300)

    def _request(self, method, key, headers=None, body=None, payload_sha256=EMPTY_SHA256, expect=(200,)):
        """Send a request, retrying connection failures, 429 and 5xx. Returns (connection, response) for a status in
        expect, 404 or 412, with the body unread; the caller closes the connection. body is bytes or a Path."""
        path = self._path(key)
        for attempt in range(ATTEMPTS):
            headers_sent = {'Host': self.host, **(headers or {})}
            if body is not None:
                headers_sent['Content-Length'] = str(len(body) if isinstance(body, bytes) else Path(body).stat().st_size)
            signed = authorization(method, self.host, path, {}, {k: v for k, v in headers_sent.items() if k != 'Host'},
                                   payload_sha256, self.access_key_id, self.secret_access_key, self.clock())
            conn = self._connection()
            try:
                target = urllib.parse.quote(path, safe='/~')
                if isinstance(body, Path):
                    with body.open('rb') as stream:
                        conn.request(method, target, body=stream, headers={**headers_sent, **signed})
                        response = conn.getresponse()
                else:
                    conn.request(method, target, body=body, headers={**headers_sent, **signed})
                    response = conn.getresponse()
            except (OSError, http.client.HTTPException):
                conn.close()
                if attempt + 1 == ATTEMPTS:
                    raise
                self.sleep(2 ** attempt)
                continue
            if response.status in expect or response.status in (404, 412):
                return conn, response
            error = response.read() if method != 'HEAD' else b''
            conn.close()
            if (response.status == 429 or response.status >= 500) and attempt + 1 < ATTEMPTS:
                self.sleep(2 ** attempt)
                continue
            raise R2Error(method, key, response.status, _error_code(error))
        raise AssertionError('unreachable')

    def _read(self, method, key, consume):
        """consume(response) for a read, or None if there's no object. A read that fails partway, after its status,
        is tried again from the start, as a failed request is."""
        for attempt in range(ATTEMPTS):
            conn, response = self._request(method, key)
            try:
                if response.status == 404:
                    response.read()
                    return None
                return consume(response)
            except (OSError, http.client.HTTPException):
                if attempt + 1 == ATTEMPTS:
                    raise
                self.sleep(2 ** attempt)
            finally:
                conn.close()
        raise AssertionError('unreachable')

    def head(self, key):
        """An object's size, ETag and recorded SHA-256, or None if there's none."""
        def consume(response):
            response.read()
            return {'bytes': int(response.getheader('Content-Length')), 'etag': response.getheader('ETag'),
                    'sha256': response.getheader('x-amz-meta-sha256'),
                    'cache_control': response.getheader('Cache-Control')}
        return self._read('HEAD', key, consume)

    def get(self, key):
        """An object's contents and ETag, read in one request, or None if there's none."""
        def consume(response):
            data = response.read()
            length = response.getheader('Content-Length')
            if length is not None and int(length) != len(data):
                raise http.client.IncompleteRead(data, int(length) - len(data))
            return data, response.getheader('ETag')
        return self._read('GET', key, consume)

    def digest(self, key):
        """An object's size and SHA-256, streamed, or None if there's none."""
        def consume(response):
            digest, size = hashlib.sha256(), 0
            for chunk in iter(lambda: response.read(1024 * 1024), b''):
                digest.update(chunk)
                size += len(chunk)
            length = response.getheader('Content-Length')
            if length is not None and int(length) != size:
                raise http.client.IncompleteRead(b'', int(length) - size)
            return size, digest.hexdigest()
        return self._read('GET', key, consume)

    def put(self, key, body, content_type, cache_control, sha256=None, if_none_match=False, if_match=None):
        """Write body, bytes or a Path, and return its ETag. Raises PreconditionFailed if a condition fails."""
        if sha256 is None:
            sha256 = hashlib.sha256(body).hexdigest() if isinstance(body, bytes) else identity(body)['sha256']
        headers = {'Content-Type': content_type, 'Cache-Control': cache_control, 'x-amz-meta-sha256': sha256}
        if if_none_match:
            headers['If-None-Match'] = '*'
        if if_match is not None:
            # One strong, quoted ETag, as R2 gives: anything else would make the write unconditional or never match.
            if not re.fullmatch(r'"[^"\s]+"', if_match):
                raise ValueError('If-Match needs one strong, quoted ETag')
            headers['If-Match'] = if_match
        conn, response = self._request('PUT', key, headers, body, sha256)
        try:
            response.read()
            if response.status == 412:
                raise PreconditionFailed(key)
            if response.status == 404:
                raise R2Error('PUT', key, 404, 'NoSuchBucket')
            return response.getheader('ETag')
        finally:
            conn.close()


def put_immutable(r2, path, key, content_type):
    """Upload a file that must never change. A retry finds the same object and accepts it; a different one is a
    conflict."""
    expected = identity(path)
    try:
        r2.put(key, Path(path), content_type, IMMUTABLE, sha256=expected['sha256'], if_none_match=True)
        return expected
    except PreconditionFailed:
        pass
    existing = r2.head(key)
    if existing is None or existing['bytes'] != expected['bytes'] or existing['sha256'] != expected['sha256']:
        raise RuntimeError(f'immutable object conflict: {key}')
    return expected
