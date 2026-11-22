"""GitHub's REST API, with the standard library alone: the runner service's machines have no gh.

The token comes from GITHUB_TOKEN or GH_TOKEN, and the repository from GITHUB_REPOSITORY. Neither is printed.
"""
import json
import os
import re
import urllib.error
import urllib.request

API = 'https://api.github.com'


class GitHubError(RuntimeError):
    def __init__(self, method, path, status):
        super().__init__(f'GitHub answered {method} {path} with {status}')
        self.status = status


def repository(env=os.environ):
    name = env.get('GITHUB_REPOSITORY', '')
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', name):
        raise SystemExit('GITHUB_REPOSITORY must name the repository, owner/name')
    return name


def _token(env=os.environ):
    token = env.get('GITHUB_TOKEN') or env.get('GH_TOKEN')
    if not token:
        raise SystemExit('GITHUB_TOKEN must be set to call GitHub')
    return token


def request(method, path, body=None, env=os.environ):
    """(status, decoded JSON, next page's URL or None) for an API path such as repos/o/r/git/refs, or a full URL."""
    url = path if path.startswith(API + '/') else f'{API}/{path}'
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method, headers={
        'Accept': 'application/vnd.github+json', 'Authorization': f'Bearer {_token(env)}',
        'X-GitHub-Api-Version': '2022-11-28', **({'Content-Type': 'application/json'} if data else {})})
    try:
        with urllib.request.urlopen(req, timeout=60) as response:
            payload = response.read()
            links = response.headers.get('Link', '')
            status = response.status
    except urllib.error.HTTPError as error:
        raise GitHubError(method, url.removeprefix(API + '/'), error.code) from None
    match = re.search(r'<([^>]+)>;\s*rel="next"', links)
    return status, json.loads(payload) if payload else None, match[1] if match else None


def paginate(path, env=os.environ):
    """Every item of a list endpoint, following its pages."""
    items, url = [], f'{path}{"&" if "?" in path else "?"}per_page=100'
    while url:
        _, page, url = request('GET', url, env=env)
        items.extend(page)
    return items
