import XCTest
@testable import Codexpress

final class SSHRemoteTests: XCTestCase {
    func testLocalSSHConfigDiscoversExplicitAliasesAcrossIncludesWithoutExecutingCommands() throws {
        let home = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: home) }
        let ssh = home.appendingPathComponent(".ssh")
        try FileManager.default.createDirectory(at: ssh.appendingPathComponent("hosts"), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: ssh.appendingPathComponent("with spaces"), withIntermediateDirectories: true)
        try """
        Host=alpha spare # comment
            User worker
            IdentityFile /not-read/private-key
        Include hosts/*.conf "with spaces/extra.conf"
        Include ~/.ssh/hosts/more.conf missing-*.conf
        Host * !blocked stage-? stage-* [abc] -invalid
        Match exec "touch \(home.path)/must-not-exist"
            ProxyCommand touch \(home.path)/must-not-exist
        hOsT "quoted-host" ALPHA
        """.write(to: ssh.appendingPathComponent("config"), atomically: true, encoding: .utf8)
        try "Host beta backup\nInclude config\n".write(to: ssh.appendingPathComponent("hosts/main.conf"), atomically: true, encoding: .utf8)
        try "Host gamma\n".write(to: ssh.appendingPathComponent("hosts/more.conf"), atomically: true, encoding: .utf8)
        try "Host delta\n".write(to: ssh.appendingPathComponent("with spaces/extra.conf"), atomically: true, encoding: .utf8)
        let catalog = SSHConfigCatalog.load(home: home)
        XCTAssertEqual(catalog.hosts, ["alpha", "backup", "beta", "delta", "gamma", "quoted-host", "spare"])
        XCTAssertTrue(catalog.exists); XCTAssertFalse(catalog.incomplete)
        XCTAssertFalse(FileManager.default.fileExists(atPath: home.appendingPathComponent("must-not-exist").path))
    }
    func testLocalSSHConfigHandlesMissingFilesSymlinksAndOversizedIncludes() throws {
        let home = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: home) }
        let ssh = home.appendingPathComponent(".ssh")
        XCTAssertFalse(SSHConfigCatalog.load(home: home).exists)
        try FileManager.default.createDirectory(at: ssh, withIntermediateDirectories: true)
        try Data().write(to: ssh.appendingPathComponent("config"))
        let empty = SSHConfigCatalog.load(home: home)
        XCTAssertTrue(empty.exists); XCTAssertFalse(empty.incomplete); XCTAssertTrue(empty.hosts.isEmpty)
        try FileManager.default.removeItem(at: ssh.appendingPathComponent("config"))
        let shared = home.appendingPathComponent("shared-config")
        try "Host available\nInclude huge.conf\n".write(to: shared, atomically: true, encoding: .utf8)
        try FileManager.default.createSymbolicLink(at: ssh.appendingPathComponent("config"), withDestinationURL: shared)
        try Data(repeating: 65, count: 1024 * 1024 + 1).write(to: ssh.appendingPathComponent("huge.conf"))
        let catalog = SSHConfigCatalog.load(home: home)
        XCTAssertEqual(catalog.hosts, ["available"])
        XCTAssertTrue(catalog.exists); XCTAssertTrue(catalog.incomplete)
    }
    @MainActor func testSelectingConfiguredHostUsesSSHDefaultsAndKeepsMachineMetadataIsolated() {
        let model = SSHRemoteModel()
        model.profile.destination = "old-server"; model.profile.codexHome = "/old/.codex"; model.profile.backupID = "old-backup"
        model.profile.username = "old-user"; model.profile.sshPort = "2222"; model.profile.identityFile = "/old/key"
        let previousID = model.profile.id
        model.profile.destination = "new-server" // Editing the field does not identify the saved machine.
        model.selectConfiguredHost("new-server", saved: [])
        XCTAssertNotEqual(model.profile.id, previousID)
        XCTAssertEqual(model.profile.destination, "new-server"); XCTAssertEqual(model.profile.codexHome, ""); XCTAssertNil(model.profile.backupID)
        var saved = SSHRemoteProfile(); saved.destination = "known-server"; saved.codexHome = "/known/.codex"; saved.backupID = "known-backup"
        saved.username = "override"; saved.sshPort = "2222"; saved.identityFile = "/some/key"
        model.selectConfiguredHost("known-server", saved: [saved])
        XCTAssertEqual(model.profile.id, saved.id); XCTAssertEqual(model.profile.backupID, "known-backup")
        XCTAssertEqual(model.profile.codexHome, "/known/.codex")
        XCTAssertEqual(model.profile.username, ""); XCTAssertEqual(model.profile.sshPort, ""); XCTAssertEqual(model.profile.identityFile, "")
        let args = try? model.profile.arguments()
        XCTAssertFalse(args?.contains("-l") ?? true); XCTAssertFalse(args?.contains("-p") ?? true); XCTAssertFalse(args?.contains("-i") ?? true)
    }
    func testSSHArgumentsUseKnownHostsAndSeparateOptionsWithoutShellInterpolation() throws {
        var profile=SSHRemoteProfile();profile.destination="server-alias";profile.username="worker";profile.sshPort="2222";profile.identityFile="/tmp/test key"
        let args=try profile.arguments()
        XCTAssertTrue(args.contains("BatchMode=yes"));XCTAssertTrue(args.contains("StrictHostKeyChecking=yes"))
        XCTAssertEqual(Array(args.suffix(3)).first,"--");XCTAssertTrue(args.contains("/tmp/test key"));XCTAssertTrue(args.contains("worker"))
        for target in ["-oProxyCommand=bad","host;touch /tmp/x","user@host","host\ncommand"] {profile.destination=target;XCTAssertThrowsError(try profile.arguments())}
        profile.destination="host";profile.sshPort="65536";XCTAssertThrowsError(try profile.arguments())
    }
    func testRemoteAgentDiscoveryApplyBackupConflictAndRestoreWithLocalFixture() throws {
        let program=SSHRemoteAgent.script.data(using:.utf8)!.base64EncodedString()
        let harness = #"""
import base64, http.server, json, os, pathlib, subprocess, sys, tempfile, threading
agent=base64.b64decode(sys.argv[1]).decode()
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        assert self.headers['Authorization']=='Bearer fixture-remote-key'
        self.send_response(200);self.end_headers();self.wfile.write(b'{"service":"codexpress","mode":"pool"}')
    def log_message(self,*args):pass
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
threading.Thread(target=server.serve_forever,daemon=True).start()
with tempfile.TemporaryDirectory() as folder:
    home=pathlib.Path(folder);codex=home/'.codex';codex.mkdir()
    config=codex/'config.toml';auth=codex/'auth.json'
    original=b'model="fixture"\n[mcp_servers.test]\ncommand="test"\n'
    config.write_bytes(original);auth.write_bytes(b'private-fixture-auth')
    env=dict(os.environ,HOME=folder,CODEX_HOME=str(codex),SSH_CONNECTION='192.0.2.44 44000 192.0.2.1 22',HTTP_PROXY='http://127.0.0.1:1',http_proxy='http://127.0.0.1:1')
    def call(action,**fields):
        payload=dict(action=action,codex_home=str(codex),**fields)
        result=subprocess.run([sys.executable,'-c',agent],input=json.dumps(payload).encode(),capture_output=True,env=env,timeout=10)
        assert result.returncode==0 and not result.stderr
        return json.loads(result.stdout)
    found=call('discover')['data'];assert found['client_ip']=='192.0.2.44' and found['candidates']==[str(codex)]
    before=call('read')['data'];assert base64.b64decode(before['config'])==original
    assert b'private-fixture-auth' not in json.dumps(before).encode()
    fields=dict(base_url='http://127.0.0.1:%d/v1'%server.server_port,key='fixture-remote-key',expected_digest=before['digest'],config=base64.b64encode(b'model_provider="codexpress"\n').decode())
    assert not call('apply',**dict(fields,base_url='http://127.0.0.1:1/v1'))['ok'] and config.read_bytes()==original
    result=call('apply',**fields);assert result['ok'];backup=result['data']['backup_id']
    assert result['data']['original_backup_id']==backup
    assert auth.read_bytes()==b'private-fixture-auth'
    assert config.read_bytes()==b'model_provider="codexpress"\n'
    assert config.stat().st_mode&0o777==0o600
    assert (codex/'.codexpress-backups'/backup/'auth.json').read_bytes()==b'private-fixture-auth'
    assert not call('apply',**fields)['ok']
    # Reapplying a different gateway configuration must not replace the original.
    gateway2=b'model_provider="codexpress"\nmodel="other-fixture"\n'
    fields2=dict(fields,expected_digest=call('read')['data']['digest'],config=base64.b64encode(gateway2).decode())
    applied2=call('apply',**fields2);assert applied2['ok'] and applied2['data']['original_backup_id']==backup
    assert applied2['data']['backup_id']!=backup and config.read_bytes()==gateway2
    auth.write_bytes(b'changed-auth')
    result=call('restore');assert result['ok'];assert config.read_bytes()==original and auth.read_bytes()==b'private-fixture-auth'
    assert result['data']['recovery_backup_id']!=backup
    assert result['data']['original_backup_id']==backup
    assert (codex/'.codexpress-backups'/result['data']['recovery_backup_id']/'config.toml').read_bytes()==gateway2
    # Repeated restore stays at the original instead of toggling back to the gateway.
    assert call('restore')['ok'] and config.read_bytes()==original
    # Legacy profiles may only remember the latest checkpoint. Recover from remote
    # history without trusting that ID, then persist the stable original marker.
    marker=codex/'.codexpress-backups/original.json';marker.unlink()
    manifest_path=codex/'.codexpress-backups'/backup/'manifest.json'
    manifest=json.loads(manifest_path.read_bytes());manifest.pop('reason');manifest.pop('created_at_ns');manifest_path.write_text(json.dumps(manifest))
    legacy=call('restore');assert legacy['ok'] and legacy['data']['original_backup_id']==backup
    assert json.loads(marker.read_bytes())['backup_id']==backup
    assert not call('restore',backup_id='../../outside')['ok']
    # Restore preflight detects corrupt backups before changing current files.
    (codex/'.codexpress-backups'/backup/'config.toml').write_bytes(b'corrupt')
    assert not call('restore')['ok'] and config.read_bytes()==original
    config.unlink();config.symlink_to(home/'external');assert not call('read')['ok']
server.shutdown()
print('remote agent fixture passed')
"""#
        let task=Process();task.executableURL=URL(fileURLWithPath:"/usr/bin/env");task.arguments=["python3","-c",harness,program]
        let out=Pipe();task.standardOutput=out;task.standardError=out
        try task.run();let output=out.fileHandleForReading.readDataToEndOfFile();task.waitUntilExit()
        XCTAssertEqual(task.terminationStatus,0,String(data:output,encoding:.utf8) ?? "Python fixture failed")
    }
    func testOriginalRemoteRestoreRefusesManagedOrUnverifiableLegacyBackups() throws {
        let program=SSHRemoteAgent.script.data(using:.utf8)!.base64EncodedString()
        let harness = #"""
import base64, hashlib, json, os, pathlib, subprocess, sys, tempfile
agent=base64.b64decode(sys.argv[1]).decode()
with tempfile.TemporaryDirectory() as folder:
    codex=pathlib.Path(folder).resolve()/'.codex';codex.mkdir()
    config=codex/'config.toml';auth=codex/'auth.json'
    current=b'model_provider="codexpress"\n';config.write_bytes(current);auth.write_bytes(b'current-private-auth')
    history=codex/'sessions';history.mkdir();(history/'fixture.json').write_bytes(b'leave history alone')
    def call():
        result=subprocess.run([sys.executable,'-c',agent],input=json.dumps(dict(action='restore',codex_home=str(codex))).encode(),capture_output=True,timeout=10)
        assert result.returncode==0 and not result.stderr
        return json.loads(result.stdout)
    assert not call()['ok']
    root=codex/'.codexpress-backups';root.mkdir()
    def checkpoint(name,value):
        target=root/name;target.mkdir();(target/'config.toml').write_bytes(value)
        files={'config.toml':hashlib.sha256(value).hexdigest(),'auth.json':'absent','auth.json.bak':'absent'}
        (target/'manifest.json').write_text(json.dumps(dict(home=str(codex),files=files)))
    first='20000101-000001-000000000001';second='20000101-000002-000000000002'
    checkpoint(first,current);checkpoint(second,b'model="looks-clean-but-is-later"\n')
    assert not call()['ok'], 'must not skip an already-managed earliest backup and call a later one original'
    assert not (root/'original.json').exists()
    assert config.read_bytes()==current and auth.read_bytes()==b'current-private-auth'
    (root/first/'manifest.json').unlink()
    assert not call()['ok'], 'an incomplete earliest backup must not be skipped for a later clean snapshot'
    # A recorded baseline never falls back to another checkpoint when its ID is invalid.
    (root/'original.json').write_text(json.dumps(dict(version=1,home=str(codex),backup_id='../../outside',config_digest='absent')))
    assert not call()['ok']
    assert config.read_bytes()==current and auth.read_bytes()==b'current-private-auth'
    assert (history/'fixture.json').read_bytes()==b'leave history alone'
print('original restore safeguards passed')
"""#
        let task=Process();task.executableURL=URL(fileURLWithPath:"/usr/bin/env");task.arguments=["python3","-c",harness,program]
        let out=Pipe();task.standardOutput=out;task.standardError=out
        try task.run();let output=out.fileHandleForReading.readDataToEndOfFile();task.waitUntilExit()
        XCTAssertEqual(task.terminationStatus,0,String(data:output,encoding:.utf8) ?? "Python fixture failed")
    }
}
