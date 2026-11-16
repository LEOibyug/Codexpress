import XCTest
@testable import Codexpress

final class SSHRemoteOMPTests: XCTestCase {
    func testOfficialExtensionInstallAddressUpdatesAndNativeLoginBoundaries() throws {
        try runHarness(#"""
import base64, hashlib, http.server, io, json, os, pathlib, shutil, sqlite3, subprocess, sys, tarfile, tempfile, threading
agent = base64.b64decode(sys.argv[1]).decode()
requests = []
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        requests.append((self.server.label, self.path, self.headers.get('Authorization')))
        accepted = self.headers.get('Authorization') == 'Bearer ' + self.server.key
        self.send_response(200 if accepted else 401); self.end_headers()
        if accepted:
            value = dict(service='codexpress', mode=self.server.mode, version=1) if self.path.endswith('/remote-check') else dict(data=[])
            self.wfile.write(json.dumps(value).encode())
    def log_message(self, *args): pass
servers = []
for label, key in [('first', 'fixture-first-key'), ('second', 'fixture-second-key'), ('third', 'fixture-third-key')]:
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.label = label; server.key = key; server.mode = 'omp'; servers.append(server)
    threading.Thread(target=server.serve_forever, daemon=True).start()
# The independent CLI fixture consumes archives, persists installed packages, and performs
# authenticated native login. It does not return the agent's arguments as evidence.
cli_source = r'''
import json, os, pathlib, sqlite3, sys, tarfile, urllib.request
args = sys.argv[1:]
if args == ['--version']:
    print('18.18.0'); sys.exit(0)
root = pathlib.Path(os.environ['PI_CODING_AGENT_DIR'])
assert os.environ['OMP_PROFILE'] == os.environ['PI_PROFILE'] == 'default'
plugins = root/'plugins'; manifest = plugins/'node_modules'/'@codexpress'/'omp-extension'/'package.json'
if args == ['plugin', 'list', '--json']:
    npm = []
    if manifest.exists():
        pkg = json.loads(manifest.read_bytes()); npm.append(dict(name=pkg['name'], version=pkg['version'], enabled=True))
    print(json.dumps(dict(npm=npm, marketplace=[]))); sys.exit(0)
if len(args) == 3 and args[:2] == ['plugin', 'install']:
    prefix = '@codexpress/omp-extension@file:'
    assert args[2].startswith(prefix)
    archive = pathlib.Path(args[2][len(prefix):])
    with tarfile.open(archive) as source:
        pkg = json.load(source.extractfile('package/package.json'))
    assert pkg['name'] == '@codexpress/omp-extension'
    manifest.parent.mkdir(parents=True, exist_ok=True); manifest.write_text(json.dumps(pkg))
    (plugins/'package.json').write_text(json.dumps(dict(dependencies={pkg['name']:'file:' + str(archive)})))
    counter = root/'fixture-install-count'
    counter.write_text(str(int(counter.read_text()) + 1 if counter.exists() else 1))
    sys.exit(0)
if args == ['login', 'codexpress']:
    assert manifest.exists()
    key = os.environ['CODEXPRESS_API_KEY']; assert key not in repr(sys.argv)
    request = urllib.request.Request(os.environ['CODEXPRESS_BASE_URL'] + '/models', headers={'Authorization':'Bearer ' + key})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=3) as response: json.load(response)
    if os.environ.get('FIXTURE_LOGIN') == 'concurrent':
        (root/'codexpress.json').write_text('{"baseUrl":"http://user-edited/omp/v1","unknown":"concurrent"}')
        sys.exit(1)
    if os.environ.get('FIXTURE_LOGIN') == 'fail':
        print('deliberately secret diagnostic: ' + key, file=sys.stderr); sys.exit(1)
    if os.environ.get('FIXTURE_LOGIN') == 'normalized-fail':
        target = root/'codexpress.json'; value = json.loads(target.read_bytes())
        target.write_text(json.dumps(value, sort_keys=True, indent=4))
        sys.exit(1)
    if os.environ.get('FIXTURE_LOGIN') == 'normalize':
        profile = root/'codexpress.json'; config = json.loads(profile.read_bytes())
        profile.write_text(json.dumps({name:config[name] for name in ('baseUrl', 'stateDir') if name in config}, indent=2) + '\n')
    db = sqlite3.connect(root/'agent.db')
    db.execute('CREATE TABLE IF NOT EXISTS native_auth (provider TEXT PRIMARY KEY, token TEXT)')
    db.execute('INSERT OR REPLACE INTO native_auth VALUES (?, ?)', ('codexpress', key)); db.commit(); db.close()
    sys.exit(0)
sys.exit(2)
'''
# Enforce the production boundary: only the CLI child may open native credentials.
guard = '''
import pathlib, sys
def deny_private_open(event, args):
    if event == 'sqlite3.connect': raise AssertionError('agent accessed native credential database')
    if event == 'open' and isinstance(args[0], str):
        name = pathlib.Path(args[0]).name
        if name in ('agent.db', 'auth.json'): raise AssertionError('agent accessed native credentials')
sys.addaudithook(deny_private_open)
'''
with tempfile.TemporaryDirectory(dir=sys.argv[2]) as folder:
    home = pathlib.Path(folder).resolve(); path = home/'remote folder with spaces'/'agent'
    bin_dir = home/'.local'/'bin'; bin_dir.mkdir(parents=True)
    cli = bin_dir/'omp'; cli.write_text('#!' + sys.executable + '\n' + cli_source); cli.chmod(0o700)
    env = dict(os.environ, HOME=str(home), PATH='/usr/bin:/bin', SSH_CONNECTION='192.0.2.44 44000 192.0.2.1 22',
               HTTP_PROXY='http://127.0.0.1:1', http_proxy='http://127.0.0.1:1', TMPDIR=str(home))
    for name in ('PI_CONFIG_DIR', 'PI_CODING_AGENT_DIR', 'OMP_AGENT_DIR', 'OMP_PROFILE', 'PI_PROFILE', 'CODEXPRESS_BASE_URL', 'CODEXPRESS_API_KEY', 'FIXTURE_LOGIN'):
        env.pop(name, None)
    def call(action, **fields):
        payload = dict(action=action, agent_dir=str(path)); payload.update(fields)
        result = subprocess.run([sys.executable, '-c', guard + agent], input=json.dumps(payload).encode(), capture_output=True, env=env, cwd=home, timeout=20)
        assert result.returncode == 0 and not result.stderr, result.stderr
        assert not any(server.key.encode() in result.stdout for server in servers), result.stdout
        return json.loads(result.stdout)
    def ok(action, **fields):
        result = call(action, **fields); assert result['ok'], result.get('error'); return result['data']
    def fields(server): return dict(base_url='http://127.0.0.1:%d/omp/v1'%server.server_port, key=server.key)
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode='w:gz') as target:
        content = json.dumps(dict(name='@codexpress/omp-extension', version='9.8.7', omp=dict(extensions=['index.ts']))).encode()
        entry = tarfile.TarInfo('package/package.json'); entry.size = len(content); target.addfile(entry, io.BytesIO(content))
    package = archive.getvalue(); package64 = base64.b64encode(package).decode()
    discovery = ok('discover')
    assert discovery['candidates'] == [str(home/'.omp'/'agent')] and discovery['client_ip'] == '192.0.2.44'
    assert discovery['omp_executable'] == str(cli) and discovery['omp_version'] == '18.18.0'
    assert not path.exists() and not (home/'.omp').exists()
    env['PI_CODING_AGENT_DIR'] = 'relative-agent'
    assert ok('discover')['candidates'] == [str(home/'relative-agent')]
    env.update(PI_CONFIG_DIR='.custom', OMP_PROFILE='work')
    assert ok('discover')['candidates'] == [str(home/'.custom'/'profiles'/'work'/'agent')]
    env['OMP_PROFILE'] = '../invalid'
    assert ok('discover')['candidates'] == [str(home/'.omp'/'agent')]
    for name in ('PI_CONFIG_DIR', 'OMP_PROFILE', 'PI_CODING_AGENT_DIR'): env.pop(name)
    initial = ok('status')
    assert initial == dict(agent_dir=str(path), omp_executable=str(cli), omp_version='18.18.0', installed=False, environment_override=False)
    assert not path.exists()
    # Probe and failed installation preflight never create a new profile/directory.
    servers[0].mode = 'pool'
    assert not call('probe', **fields(servers[0]))['ok']
    assert not call('install', package=package64, **fields(servers[0]))['ok'] and not path.exists()
    servers[0].mode = 'omp'
    assert ok('probe', **fields(servers[0])) == dict(connected=True) and not path.exists()
    installed = ok('install', package=package64, **fields(servers[0]))
    expected_fields = {'agent_dir', 'omp_executable', 'omp_version', 'installed', 'extension_version', 'base_url', 'environment_override', 'connected'}
    assert set(installed) == expected_fields and installed['installed'] and installed['connected']
    assert installed['extension_version'] == '9.8.7' and installed['base_url'] == fields(servers[0])['base_url']
    profile = path/'codexpress.json'
    assert json.loads(profile.read_bytes()) == dict(baseUrl=fields(servers[0])['base_url'])
    db = sqlite3.connect(path/'agent.db'); assert db.execute('SELECT token FROM native_auth').fetchone() == (servers[0].key,); db.close()
    assert (path/'fixture-install-count').read_text() == '1'
    persistent = path/'.codexpress'/'packages'/('codexpress-' + hashlib.sha256(package).hexdigest() + '.tgz')
    assert persistent.read_bytes() == package and str(persistent) in (path/'plugins'/'package.json').read_text()
    assert not any((path/name).exists() for name in ('models.yml', 'models.yaml', 'config.yml', 'settings.json', 'auth.json'))
    config = dict(baseUrl=installed['base_url'], unknown=dict(nested=[1, '用户字段']), stateDir='/user/state')
    profile.write_text(json.dumps(config, ensure_ascii=False))
    # Two address/key changes must reach the new listeners and not reinstall the package.
    env.update(CODEXPRESS_BASE_URL=fields(servers[0])['base_url'], OMP_PROFILE='wrong-profile', PI_PROFILE='wrong-profile', PI_CODING_AGENT_DIR=str(home/'wrong-agent'))
    for server in servers[1:]:
        env['FIXTURE_LOGIN'] = 'normalize' if server is servers[1] else ''
        count = len(requests); changed = ok('configure', **fields(server))
        assert changed['base_url'] == fields(server)['base_url'] and changed['connected'] and changed['environment_override']
        assert requests[count:] == [(server.label, '/omp/v1/remote-check', 'Bearer ' + server.key), (server.label, '/omp/v1/models', 'Bearer ' + server.key)]
        config['baseUrl'] = fields(server)['base_url']; assert json.loads(profile.read_bytes()) == config
        assert (path/'fixture-install-count').read_text() == '1' and not (home/'wrong-agent').exists()
    assert ok('status')['environment_override']
    env['CODEXPRESS_BASE_URL'] = fields(servers[2])['base_url']; assert not ok('status')['environment_override']
    baseline = profile.read_bytes(); package_baseline = persistent.read_bytes()
    # Refused connection, failed authentication and wrong identity cannot change any profile bytes.
    dead = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler); dead_port = dead.server_port; dead.server_close()
    for invalid in (dict(base_url='http://127.0.0.1:%d/omp/v1'%dead_port, key='offline-key'), dict(fields(servers[1]), key='wrong-key')):
        for action in ('probe', 'configure'):
            result = call(action, **invalid); assert not result['ok'] and profile.read_bytes() == baseline
    servers[1].mode = 'other'
    assert not call('configure', **fields(servers[1]))['ok'] and profile.read_bytes() == baseline
    servers[1].mode = 'omp'
    for mode in ('fail', 'normalized-fail'):
        env['FIXTURE_LOGIN'] = mode
        failed = call('configure', **fields(servers[1]))
        assert not failed['ok'] and '已恢复原地址配置' in failed['error'] and profile.read_bytes() == baseline
    # A failed first login removes the newly written profile rather than reporting success.
    profile.unlink()
    assert not call('configure', **fields(servers[1]))['ok'] and not profile.exists()
    profile.write_bytes(baseline)
    env['FIXTURE_LOGIN'] = 'concurrent'
    failed = call('configure', **fields(servers[1]))
    assert not failed['ok'] and '已保留' in failed['error']
    assert json.loads(profile.read_bytes())['unknown'] == 'concurrent'
    env.pop('FIXTURE_LOGIN'); profile.write_bytes(baseline)
    assert persistent.read_bytes() == package_baseline and (path/'fixture-install-count').read_text() == '1'
    # A later native reinstall still resolves the persistent file after prior processes exited.
    shutil.rmtree(path/'plugins')
    native_env = dict(env, PI_CODING_AGENT_DIR=str(path), OMP_PROFILE='default', PI_PROFILE='default')
    process = subprocess.run([str(cli), 'plugin', 'install', '@codexpress/omp-extension@file:' + str(persistent)], env=native_env, capture_output=True, timeout=10)
    assert process.returncode == 0, process.stderr
    assert ok('status')['installed'] and (path/'fixture-install-count').read_text() == '2'
    # An uninstalled configure cannot leave a new profile claiming success.
    shutil.rmtree(path/'plugins'); profile.unlink()
    assert not call('configure', **fields(servers[0]))['ok'] and not profile.exists()
    assert persistent.read_bytes() == package
for server in servers: server.shutdown(); server.server_close()
print('OMP native extension boundary fixture passed')
"""#)
    }

    func testLegacyReadRestoreRetainsMigrationAndPathBoundaries() throws {
        try runHarness(#"""
import base64, json, os, pathlib, subprocess, sys, tempfile
agent = base64.b64decode(sys.argv[1]).decode()
with tempfile.TemporaryDirectory(dir=sys.argv[2]) as folder:
    home = pathlib.Path(folder).resolve(); root = home/'agent'
    env = dict(os.environ, HOME=str(home), TMPDIR=str(home))
    def call(action, **fields):
        payload = dict(action=action, agent_dir=str(root)); payload.update(fields)
        process = subprocess.run([sys.executable, '-c', agent], input=json.dumps(payload).encode(), env=env, cwd=home, capture_output=True, timeout=10)
        assert process.returncode == 0 and not process.stderr, process.stderr
        return json.loads(process.stdout)
    def enc(value): return base64.b64encode(value).decode()
    before = call('read')['data']; assert not root.exists() and before['snapshot'] is None
    root.mkdir(); (root/'models.yml').write_bytes(b'legacy managed models\n'); (root/'config.yml').write_bytes(b'legacy managed config\n')
    (root/'settings.json').write_bytes(b'{"user":true}')
    meta = root/'.codexpress'; meta.mkdir(); (meta/'attachment.json').write_bytes(b'{"legacy":"snapshot"}')
    (root/'agent.db').mkdir(); (root/'agent.db'/'private').write_bytes(b'never read')
    (root/'auth.json').symlink_to(home/'missing-auth')
    current = call('read')['data']
    assert current['agent_db_exists'] and b'never read' not in json.dumps(current).encode()
    target = dict(current['files'], **{'models.yml':None, 'config.yml':None})
    # Stale previews preserve external edits; migration restore requires no gateway or key.
    (root/'settings.json').write_bytes(b'{"user":"edited"}')
    assert not call('restore', files=target, snapshot=None, expected_digest=current['digest'])['ok']
    current = call('read')['data']; target['settings.json'] = current['files']['settings.json']
    restored = call('restore', files=target, snapshot=None, expected_digest=current['digest'])
    assert restored['ok'], restored
    assert not (root/'models.yml').exists() and not (root/'config.yml').exists() and not (meta/'attachment.json').exists()
    assert (root/'settings.json').read_bytes() == b'{"user":"edited"}' and (root/'agent.db'/'private').read_bytes() == b'never read'
    backup = json.loads((meta/'backups'/restored['data']['backup_id']/'bundle.json').read_bytes())
    assert backup['snapshot'] == current['snapshot'] and 'auth.json' not in backup['files']
    current = call('read')['data']
    assert not call('restore', files=dict(target, **{'../outside':enc(b'bad')}), snapshot=None, expected_digest=current['digest'])['ok']
    assert not call('read', agent_dir=str(root/'..'/'escape'))['ok'] and not (home/'outside').exists()
    (root/'config.yml').symlink_to(home/'external'); assert not call('read')['ok'] and not (home/'external').exists()
    (root/'config.yml').unlink(); external = home/'external'; external.write_bytes(b'private fixture'); os.link(external, root/'config.yml')
    assert not call('read')['ok'] and external.read_bytes() == b'private fixture'
    (root/'config.yml').unlink(); alias = home/'alias'; alias.symlink_to(root, target_is_directory=True)
    assert not call('read', agent_dir=str(alias))['ok']
print('OMP legacy migration boundary fixture passed')
"""#)
    }

    func testOMPBackupConflictAndFailedWriteNeverRollBackConcurrentEdits() throws {
        try runHarness(#"""
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
with tempfile.TemporaryDirectory(dir=sys.argv[2]) as folder:
    root = pathlib.Path(folder).resolve()/'agent'; root.mkdir()
    (root/'models.yml').write_bytes(b'original models\n'); (root/'config.yml').write_bytes(b'original config\n')
    env = dict(os.environ, HOME=str(pathlib.Path(folder).resolve()), FIXTURE_ROOT=str(root), TMPDIR=folder)
    def call(action, race='', **fields):
        payload = dict(action=action, agent_dir=str(root), **fields)
        process = subprocess.run([sys.executable, '-c', prefix + agent], input=json.dumps(payload).encode(), env=dict(env, FIXTURE_RACE=race), cwd=folder, capture_output=True, timeout=10)
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
"""#)
    }

    private func runHarness(_ harness: String) throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let fixtures = root.appendingPathComponent("tmp/remote-omp-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: fixtures, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: fixtures) }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["python3", "-c", harness, Data(SSHRemoteOMPAgent.script.utf8).base64EncodedString(), fixtures.path]
        process.environment = ProcessInfo.processInfo.environment.merging(["TMPDIR": fixtures.path, "PYTHONDONTWRITEBYTECODE": "1"]) { _, new in new }
        process.currentDirectoryURL = root
        let output = Pipe(); process.standardOutput = output; process.standardError = output
        try process.run()
        let data = output.fileHandleForReading.readDataToEndOfFile(); process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0, String(data: data, encoding: .utf8) ?? "Python fixture failed")
    }
}
