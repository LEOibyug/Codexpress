import XCTest
@testable import Codexpress

final class SSHRemoteOMPTests: XCTestCase {
    func testOMPAgentTransportFreshInstallConflictUpdateRestoreAndPathGuards() throws {
        let program = Data(SSHRemoteOMPAgent.script.utf8).base64EncodedString()
        // Exercise the shipped fixed Python program, not a mock SSH client or real user state.
        let harness = #"""
import base64, http.server, json, os, pathlib, stat, subprocess, sys, tempfile, threading
agent = base64.b64decode(sys.argv[1]).decode()
identity = dict(service='codexpress', mode='chatbot', version=1)
requests = []
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        requests.append((self.path, self.headers.get('Authorization')))
        self.send_response(200); self.end_headers(); self.wfile.write(json.dumps(identity).encode())
    def log_message(self, *args): pass
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory() as folder:
    home = pathlib.Path(folder).resolve()
    path = home/'fresh'/'agent'
    env = dict(os.environ, HOME=str(home), XDG_CONFIG_HOME=str(home/'xdg'), SSH_CONNECTION='192.0.2.44 44000 192.0.2.1 22', HTTP_PROXY='http://127.0.0.1:1', http_proxy='http://127.0.0.1:1')
    for name in ('PI_CONFIG_DIR', 'PI_CODING_AGENT_DIR', 'OMP_AGENT_DIR', 'OMP_PROFILE', 'PI_PROFILE'): env.pop(name, None)
    def call(action, **fields):
        payload = dict(action=action, agent_dir=str(path)); payload.update(fields)
        result = subprocess.run([sys.executable, '-c', agent], input=json.dumps(payload).encode(), capture_output=True, env=env, cwd=str(home), timeout=15)
        assert result.returncode == 0 and not result.stderr, result.stderr
        return json.loads(result.stdout)
    def ok(action, **fields):
        result = call(action, **fields)
        assert result['ok'], result.get('error')
        return result['data']
    def enc(value): return base64.b64encode(value).decode()
    found = ok('discover')
    assert found['candidates'] == [str(home/'.omp'/'agent')] and found['client_ip'] == '192.0.2.44'
    assert not (home/'.omp').exists() and not (home/'xdg').exists()
    env['PI_CODING_AGENT_DIR'] = 'relative-agent'
    assert ok('discover')['candidates'] == [str(home/'relative-agent')]
    env['PI_CONFIG_DIR'] = '.custom'; env['OMP_PROFILE'] = 'work'
    assert ok('discover')['candidates'] == [str(home/'.custom'/'profiles'/'work'/'agent')]
    env['OMP_PROFILE'] = '../invalid'
    assert ok('discover')['candidates'] == [str(home/'.omp'/'agent')]
    before = ok('read', agent_dir=str(path.parent) + '//./agent')
    assert before['agent_dir'] == str(path) and not path.exists()
    names = ('models.yml', 'models.yaml', 'models.json', 'config.yml', 'config.yaml', 'settings.json')
    assert before['files'] == dict.fromkeys(names) and before['snapshot'] is None and not before['agent_db_exists']
    files = dict(before['files'], **{'models.yml': enc(b'providers: {}\n'), 'config.yml': enc(b'model: fixture-one\n')})
    snapshot = enc(b'{"fixture":"first-baseline"}')
    fields = dict(files=files, snapshot=snapshot, expected_digest=before['digest'], base_url='http://127.0.0.1:%d/v1'%server.server_port, key='fixture-shared-chatbot-key')
    # Wrong listener identity cannot create a missing agent directory.
    identity['mode'] = 'pool'
    assert not call('apply', **fields)['ok'] and not path.exists()
    identity['mode'] = 'chatbot'; identity['version'] = 2
    assert not call('apply', **fields)['ok'] and not path.exists()
    identity['version'] = 1
    applied = ok('apply', **fields)
    assert applied['agent_dir'] == str(path)
    assert requests[-1] == ('/v1/remote-check', 'Bearer fixture-shared-chatbot-key')
    assert (path/'config.yml').read_bytes() == b'model: fixture-one\n'
    assert (path/'.codexpress'/'attachment.json').read_bytes() == b'{"fixture":"first-baseline"}'
    for directory in (path.parent, path, path/'.codexpress', path/'.codexpress'/'backups', path/'.codexpress'/'backups'/applied['backup_id']):
        assert stat.S_IMODE(directory.stat().st_mode) == 0o700
    backup = path/'.codexpress'/'backups'/applied['backup_id']/'bundle.json'
    assert json.loads(backup.read_bytes())['files'] == before['files']
    assert stat.S_IMODE(backup.stat().st_mode) == 0o600
    assert stat.S_IMODE((path/'config.yml').stat().st_mode) == 0o600
    # Digest covers every renderer input, including snapshot and database existence.
    current = ok('read'); (path/'settings.json').write_bytes(b'{"user":true}')
    assert not call('apply', **dict(fields, expected_digest=current['digest']))['ok']
    assert (path/'settings.json').read_bytes() == b'{"user":true}' and (path/'config.yml').read_bytes() == b'model: fixture-one\n'
    current = ok('read'); (path/'.codexpress'/'attachment.json').write_bytes(b'{"edited":true}')
    assert not call('apply', **dict(fields, expected_digest=current['digest']))['ok']
    assert (path/'.codexpress'/'attachment.json').read_bytes() == b'{"edited":true}'
    current = ok('read'); (path/'agent.db').mkdir(); (path/'agent.db'/'do-not-open').write_bytes(b'fixture-private-db')
    assert not call('apply', **dict(fields, expected_digest=current['digest']))['ok']
    # Second update transports opaque renderer snapshot unchanged and preserves unrelated state.
    current = ok('read'); files2 = dict(current['files'], **{'config.yml':enc(b'model: fixture-two\n')})
    second = ok('apply', **dict(fields, files=files2, snapshot=current['snapshot'], expected_digest=current['digest']))
    assert second['backup_id'] != applied['backup_id']
    assert (path/'agent.db'/'do-not-open').read_bytes() == b'fixture-private-db'
    assert b'fixture-private-db' not in json.dumps(current).encode()
    # A renderer restore bundle removes only attachment changes. No key/network is required.
    current = ok('read'); restored = dict(current['files'], **{'models.yml':None, 'config.yml':None})
    count = len(requests)
    ok('restore', files=restored, snapshot=None, expected_digest=current['digest'])
    assert len(requests) == count and not (path/'config.yml').exists() and not (path/'models.yml').exists()
    assert not (path/'.codexpress'/'attachment.json').exists()
    assert (path/'settings.json').read_bytes() == b'{"user":true}'
    # A legacy database is never read and blocks mutation until YAML migration.
    legacy = ok('read'); assert legacy['agent_db_exists']
    assert not call('apply', **dict(fields, expected_digest=legacy['digest']))['ok']
    assert not (path/'config.yml').exists()
    (path/'agent.db'/'do-not-open').unlink(); (path/'agent.db').rmdir()
    # Exact file allowlist and symlink-component guards prevent path traversal or private reads.
    current = ok('read')
    for malicious in (dict(restored, **{'../outside':enc(b'bad')}), {'config.yml':enc(b'bad')}):
        assert not call('restore', files=malicious, snapshot=None, expected_digest=current['digest'])['ok']
    assert not (home/'outside').exists()
    assert not call('read', agent_dir=str(path/'..'/'escape'))['ok']
    (path/'config.yml').symlink_to(home/'external')
    assert not call('read')['ok'] and not (home/'external').exists()
    (path/'config.yml').unlink()
    external = home/'external'; external.write_bytes(b'private fixture')
    os.link(external, path/'config.yml')
    assert not call('read')['ok'] and external.read_bytes() == b'private fixture'
    (path/'config.yml').unlink()
    alias = home/'alias'; alias.symlink_to(path, target_is_directory=True)
    assert not call('read', agent_dir=str(alias))['ok']
    # Authentication/history paths are never opened, even when they are dangling symlinks.
    (path/'auth.json').symlink_to(home/'missing-auth'); (path/'sessions').symlink_to(home/'missing-sessions')
    ok('read')
    meta = path/'.codexpress'; saved = path/'.saved-meta'; meta.rename(saved); meta.symlink_to(saved, target_is_directory=True)
    assert not call('read')['ok']
server.shutdown()
print('OMP fixed-program transport fixture passed')
"""#
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["python3", "-c", harness, program]
        let output = Pipe(); process.standardOutput = output; process.standardError = output
        try process.run()
        let data = output.fileHandleForReading.readDataToEndOfFile(); process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0, String(data: data, encoding: .utf8) ?? "Python fixture failed")
    }
    func testOMPBackupConflictAndFailedWriteNeverRollBackConcurrentEdits() throws {
        let program = Data(SSHRemoteOMPAgent.script.utf8).base64EncodedString()
        let harness = #"""
import base64, json, os, pathlib, subprocess, sys, tempfile
agent = base64.b64decode(sys.argv[1]).decode()
# Fault injection at the OS boundary keeps the actual shipped program unchanged.
prefix = '''
import os, pathlib, sys
triggered = False
def concurrent_writer(event, args):
    global triggered
    if triggered: return
    root = pathlib.Path(os.environ['FIXTURE_ROOT'])
    mode = os.environ.get('FIXTURE_RACE')
    if mode == 'backup' and event == 'open' and args[0] == 'bundle.json':
        triggered = True
        (root/'config.yml').write_bytes(b'user edit during backup\\n')
    elif mode == 'rollback' and event == 'os.rename' and args[1] == 'config.yml':
        triggered = True
        (root/'models.yml').write_bytes(b'user edit during failed write\\n')
        raise OSError('fixture disk failure')
sys.addaudithook(concurrent_writer)
'''
with tempfile.TemporaryDirectory() as folder:
    root = pathlib.Path(folder).resolve()/'agent'; root.mkdir()
    (root/'models.yml').write_bytes(b'original models\n'); (root/'config.yml').write_bytes(b'original config\n')
    env = dict(os.environ, HOME=str(pathlib.Path(folder).resolve()), FIXTURE_ROOT=str(root))
    def call(action, race='', **fields):
        payload = dict(action=action, agent_dir=str(root), **fields)
        process = subprocess.run([sys.executable, '-c', prefix + agent], input=json.dumps(payload).encode(), env=dict(env, FIXTURE_RACE=race), capture_output=True, timeout=10)
        assert process.returncode == 0 and not process.stderr, process.stderr
        return json.loads(process.stdout)
    def enc(value): return base64.b64encode(value).decode()
    before = call('read')['data']
    target = dict(before['files'], **{'models.yml':enc(b'next models\n'), 'config.yml':enc(b'next config\n')})
    result = call('restore', race='backup', files=target, snapshot=None, expected_digest=before['digest'])
    assert not result['ok']
    assert (root/'config.yml').read_bytes() == b'user edit during backup\n'
    assert (root/'models.yml').read_bytes() == b'original models\n'
    before = call('read')['data']
    result = call('restore', race='rollback', files=target, snapshot=None, expected_digest=before['digest'])
    assert not result['ok']
    assert (root/'models.yml').read_bytes() == b'user edit during failed write\n'
    assert (root/'config.yml').read_bytes() == b'user edit during backup\n'
    backups = list((root/'.codexpress'/'backups').glob('*/bundle.json'))
    assert len(backups) == 2
    assert all('auth.json' not in json.loads(p.read_bytes())['files'] for p in backups)
print('OMP concurrent edits fixture passed')
"""#
        let process = Process(); process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["python3", "-c", harness, program]
        let output = Pipe(); process.standardOutput = output; process.standardError = output
        try process.run()
        let data = output.fileHandleForReading.readDataToEndOfFile(); process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0, String(data: data, encoding: .utf8) ?? "Python fixture failed")
    }
}
