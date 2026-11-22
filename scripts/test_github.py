"""GitHub's API without gh: requests carry the token, pages are followed, refusals keep their status."""
import io
import json
import unittest
import urllib.error
from unittest.mock import patch
import github

ENV = {'GITHUB_TOKEN': 'TOKEN', 'GITHUB_REPOSITORY': 'lanescape/browser'}


class Response(io.BytesIO):
    def __init__(self, payload, link=''):
        super().__init__(json.dumps(payload).encode())
        self.status, self.headers = 200, {'Link': link}

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False


class API(unittest.TestCase):
    def test_pages_are_followed(self):
        pages = [Response([1, 2], '<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=2>; rel="last"'),
                 Response([3])]
        with patch.object(github.urllib.request, 'urlopen', side_effect=pages) as urlopen:
            self.assertEqual(github.paginate('repos/lanescape/browser/git/matching-refs/tags/', env=ENV), [1, 2, 3])
        first, second = (call.args[0] for call in urlopen.call_args_list)
        self.assertEqual(first.full_url, 'https://api.github.com/repos/lanescape/browser/git/matching-refs/tags/?per_page=100')
        self.assertEqual(first.get_header('Authorization'), 'Bearer TOKEN')
        self.assertEqual(second.full_url, 'https://api.github.com/x?page=2')

    def test_a_refusal_keeps_its_status_and_not_the_token(self):
        error = urllib.error.HTTPError('https://api.github.com/x', 422, 'Unprocessable', {}, io.BytesIO(b'{}'))
        with patch.object(github.urllib.request, 'urlopen', side_effect=error):
            with self.assertRaises(github.GitHubError) as raised:
                github.request('POST', 'repos/lanescape/browser/git/refs', {'ref': 'refs/tags/x'}, env=ENV)
        self.assertEqual(raised.exception.status, 422)
        self.assertNotIn('TOKEN', str(raised.exception))

    def test_the_token_and_repository_are_required(self):
        with self.assertRaisesRegex(SystemExit, 'GITHUB_TOKEN'):
            github.request('GET', 'x', env={})
        with self.assertRaisesRegex(SystemExit, 'GITHUB_REPOSITORY'):
            github.repository(env={'GITHUB_REPOSITORY': 'nope'})
        self.assertEqual(github.repository(env=ENV), 'lanescape/browser')


if __name__ == '__main__':
    unittest.main()
