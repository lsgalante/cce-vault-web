#!/usr/bin/env python3
"""mock-dropbox.py <dropbox-root-dir> [port] — a stand-in for the slice of
Dropbox's API the app uses, backed by a folder: <dir>/Apps/x/Vault is the
Dropbox path /Apps/x/Vault. For testing the whole app — login, first sync,
edits both ways, conflicts, deletions — without a Dropbox account, by
pointing config.json's "endpoints" at it:

    {"app_key": "mock", "endpoints": {
        "api": "http://127.0.0.1:8001/2", "content": "http://127.0.0.1:8001/2",
        "authorize": "http://127.0.0.1:8001/oauth2/authorize",
        "token": "http://127.0.0.1:8001/oauth2/token"}}

Edit files in the folder to make "remote" changes. Revisions are derived
from content, the cursor is a snapshot of the folder, and every request is
logged to stderr. Not a faithful Dropbox: only what the app sends.
"""
import hashlib
import http.server
import json
import os
import sys
import urllib.parse

ROOT = os.path.abspath(sys.argv[1])
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8001
CURSORS = {}


def content_hash(data):
    outer = hashlib.sha256()
    for i in range(0, max(len(data), 1), 4 * 1024 * 1024):
        outer.update(hashlib.sha256(data[i:i + 4 * 1024 * 1024]).digest())
    return outer.hexdigest()


def local(path):
    return os.path.join(ROOT, path.lstrip('/'))


def meta(path):
    data = open(local(path), 'rb').read()
    h = content_hash(data)
    return {'.tag': 'file', 'path_display': path, 'path_lower': path.lower(), 'id': 'id:' + path,
            'rev': h[:16], 'content_hash': h, 'size': len(data)}


def snapshot(base):
    out = {}
    top = local(base)
    for d, _, files in os.walk(top):
        for f in files:
            p = '/' + os.path.relpath(os.path.join(d, f), ROOT)
            out[p] = meta(p)
    return out


class H(http.server.BaseHTTPRequestHandler):
    def cors(self):
        self.send_header('Access-Control-Allow-Origin', '*')
        self.send_header('Access-Control-Allow-Headers', 'Authorization, Content-Type, Dropbox-API-Arg')
        self.send_header('Access-Control-Allow-Methods', 'GET, POST, OPTIONS')
        self.send_header('Access-Control-Expose-Headers', 'Dropbox-API-Result')

    def reply(self, status, body, ctype='application/json', headers=()):
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
        self.send_response(status)
        self.cors()
        self.send_header('Content-Type', ctype)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def refuse(self, summary):
        self.reply(409, {'error_summary': summary, 'error': {'.tag': summary.split('/')[0]}})

    def do_OPTIONS(self):
        self.send_response(204)
        self.cors()
        self.end_headers()

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        if u.path == '/oauth2/authorize':
            q = urllib.parse.parse_qs(u.query)
            back = q['redirect_uri'][0] + '?' + urllib.parse.urlencode({'code': 'mockcode', 'state': q['state'][0]})
            self.send_response(302)
            self.send_header('Location', back)
            self.end_headers()
            return
        self.reply(404, {'error_summary': 'not_found'})

    def body(self):
        n = int(self.headers.get('Content-Length') or 0)
        return self.rfile.read(n) if n else b''

    def do_POST(self):
        p = urllib.parse.urlparse(self.path).path
        raw = self.body()
        arg = json.loads(self.headers.get('Dropbox-API-Arg') or 'null')
        sys.stderr.write(f'{p} {arg or raw[:120]!r}\n')
        if p == '/oauth2/token':
            return self.reply(200, {'access_token': 'mock-access', 'expires_in': 14400, 'refresh_token': 'mock-refresh',
                                    'token_type': 'bearer'})
        if self.headers.get('Authorization') != 'Bearer mock-access':
            return self.reply(401, {'error_summary': 'invalid_access_token/'})
        if p == '/2/files/list_folder':
            base = json.loads(raw)['path']
            if not os.path.isdir(local(base)):
                return self.refuse('path/not_found/')
            snap = snapshot(base)
            cur = f'c{len(CURSORS)}'
            CURSORS[cur] = (base, snap)
            return self.reply(200, {'entries': list(snap.values()), 'cursor': cur, 'has_more': False})
        if p == '/2/files/list_folder/continue':
            cur = json.loads(raw)['cursor']
            if cur not in CURSORS:
                return self.refuse('reset/')
            base, old = CURSORS[cur]
            new = snapshot(base)
            entries = [m for k, m in new.items() if old.get(k, {}).get('rev') != m['rev']]
            entries += [{'.tag': 'deleted', 'path_display': k, 'path_lower': k.lower()} for k in old if k not in new]
            nxt = f'c{len(CURSORS)}'
            CURSORS[nxt] = (base, new)
            return self.reply(200, {'entries': entries, 'cursor': nxt, 'has_more': False})
        if p == '/2/files/download':
            path = arg['path']
            if not os.path.isfile(local(path)):
                return self.refuse('path/not_found/')
            data = open(local(path), 'rb').read()
            return self.reply(200, data, 'application/octet-stream', [('Dropbox-API-Result', json.dumps(meta(path)))])
        if p == '/2/files/upload':
            path, mode = arg['path'], arg['mode']
            exists = os.path.isfile(local(path))
            if mode == 'add' and exists:
                return self.refuse('path/conflict/file/')
            if isinstance(mode, dict):
                if not exists or meta(path)['rev'] != mode['update']:
                    return self.refuse('path/conflict/file/')
            os.makedirs(os.path.dirname(local(path)), exist_ok=True)
            open(local(path), 'wb').write(raw)
            return self.reply(200, meta(path))
        if p == '/2/files/delete_v2':
            a = json.loads(raw)
            if not os.path.isfile(local(a['path'])):
                return self.refuse('path_lookup/not_found/')
            if a.get('parent_rev') and meta(a['path'])['rev'] != a['parent_rev']:
                return self.refuse('path/conflict/')
            m = meta(a['path'])
            os.remove(local(a['path']))
            return self.reply(200, {'metadata': m})
        self.reply(404, {'error_summary': 'unknown_endpoint/'})

    def log_message(self, *a):
        pass


print(f'mock Dropbox on http://127.0.0.1:{PORT}, root {ROOT}', file=sys.stderr)
http.server.ThreadingHTTPServer(('127.0.0.1', PORT), H).serve_forever()
