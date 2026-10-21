#!/usr/bin/env python3
"""Exercise the bundled image skill against localhost fixtures only."""
from pathlib import Path
import base64, http.server, json, os, subprocess, tempfile, threading, unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'Resources/skills/codexpress-image/scripts/image.py'
PNG = base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jKCkAAAAASUVORK5CYII=')

class SkillTests(unittest.TestCase):
    def test_generate_edit_check_no_proxy_no_secret_output_and_no_retry(self):
        requests = []
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_GET(self):
                self.assert_auth()
                requests.append((self.path, None))
                self.reply({'data': [{'id': 'gpt-image-2'}]})
            def assert_auth(self):
                assert self.headers['Authorization'] == 'Bearer PRIVATE-SKILL-KEY'
            def reply(self, body, status=200):
                self.send_response(status); self.send_header('Content-Type', 'application/json'); self.end_headers(); self.wfile.write(json.dumps(body).encode())
            def do_POST(self):
                self.assert_auth()
                data = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                requests.append((self.path, data))
                if data['prompt'] == 'fail': self.reply({'error':{}}, 500); return
                self.reply({'data': [{'b64_json':base64.b64encode(PNG).decode()} for _ in range(data['n'])]})
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix='cx-image-skill-') as folder:
                directory = Path(folder)
                config = directory / 'client.json'
                config.write_text(json.dumps({'base_url': f'http://127.0.0.1:{server.server_port}/v1', 'api_key':'PRIVATE-SKILL-KEY'}))
                env = os.environ | {'CODEXPRESS_IMAGE_CONFIG':str(config), 'HTTP_PROXY':'http://127.0.0.1:1', 'HTTPS_PROXY':'http://127.0.0.1:1', 'ALL_PROXY':'http://127.0.0.1:1'}
                def run(*args, ok=True):
                    p = subprocess.run(['python3', str(SCRIPT), *map(str,args)], env=env, capture_output=True, text=True, timeout=5)
                    self.assertEqual(p.returncode == 0, ok, p.stderr)
                    self.assertNotIn('PRIVATE-SKILL-KEY', p.stdout+p.stderr)
                    return p
                run('check'); self.assertEqual(len(requests),1)
                output = directory/'out.png'
                run('generate','--prompt','fixture','--out',output)
                self.assertEqual(output.read_bytes(),PNG)
                before = len(requests)
                run('generate','--prompt','fixture','--out',output,ok=False)
                self.assertEqual(len(requests),before)
                run('edit','--prompt','edit fixture','--image',output,'--out',directory/'edited.png')
                self.assertEqual(requests[-1][0],'/v1/images/edits')
                self.assertTrue(requests[-1][1]['images'][0]['image_url'].startswith('data:image/png;base64,'))
                run('generate','--prompt','variants','--n','2','--out',directory/'variants.png')
                self.assertTrue((directory/'variants-1.png').exists()); self.assertTrue((directory/'variants-2.png').exists())
                before = len(requests)
                run('generate','--prompt','fail','--out',directory/'failed.png',ok=False)
                self.assertEqual(len(requests), before+1)
        finally: server.shutdown(); server.server_close()

if __name__ == '__main__': unittest.main()
