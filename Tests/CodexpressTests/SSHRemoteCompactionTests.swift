import XCTest
@testable import Codexpress

final class SSHRemoteCompactionTests: XCTestCase {
    func testRemoteCompactionCreatesMissingFilesRestoresOriginalBytesAndProtectsPreviewConflicts() throws {
        let program = Data(SSHRemoteAgent.script.utf8).base64EncodedString()
        let harness = #"""
import base64, http.server, json, os, pathlib, subprocess, sys, tempfile, threading
agent=base64.b64decode(sys.argv[1]).decode()
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        assert self.headers['Authorization']=='Bearer fixture-remote'
        self.send_response(200);self.end_headers();self.wfile.write(b'{"service":"codexpress","mode":"pool"}')
    def log_message(self,*args):pass
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
threading.Thread(target=server.serve_forever,daemon=True).start()
for legacy in (False,True):
  with tempfile.TemporaryDirectory() as folder:
    codex=pathlib.Path(folder).resolve()/'.codex';codex.mkdir()
    config=codex/'config.toml';catalog=codex/'.codexpress-model-catalog.json';auth=codex/'auth.json'
    original=b'model="fixture"\nmodel_auto_compact_token_limit=30000\nmodel_context_window=100000\n' if legacy else None
    old_catalog=b'user-owned preexisting file' if legacy else None
    if original is not None: config.write_bytes(original);auth.write_bytes(b'original-auth');catalog.write_bytes(old_catalog)
    history=codex/'sessions';history.mkdir();(history/'keep.json').write_bytes(b'keep conversation')
    def call(action,code=agent,**fields):
        result=subprocess.run([sys.executable,'-c',code],input=json.dumps(dict(action=action,codex_home=str(codex),**fields)).encode(),capture_output=True,timeout=10)
        assert result.returncode==0 and not result.stderr
        return json.loads(result.stdout)
    common=dict(base_url='http://127.0.0.1:%d/v1'%server.server_port,key='fixture-remote')
    gateway=b'model_provider="codexpress"\n'
    if legacy:
        before=call('read')['data']
        applied=call('apply',**dict(common,expected_digest=before['digest'],config=base64.b64encode(gateway+(original or b'')).decode()))
        assert applied['ok']
        # Simulate an initial baseline created before model-catalog backup support.
        first=codex/'.codexpress-backups'/applied['data']['original_backup_id']/'manifest.json'
        manifest=json.loads(first.read_bytes());manifest['files'].pop(catalog.name);first.write_text(json.dumps(manifest))
    def fields(limit=60000):
        before=call('read')['data']
        source=call('catalog',catalog_path='missing-source.json')['data']
        content=json.dumps(dict(models=[dict(slug='fixture',auto_compact_token_limit=limit,context_window=100000)]))
        after='model_provider="codexpress"\nmodel_catalog_json='+json.dumps(str(catalog))+'\n'
        return dict(common,expected_digest=before['digest'],config=base64.b64encode(after.encode()).decode(),catalog_action='write',catalog=content,expected_catalog_digest=before['catalog_digest'],expected_compaction_state_digest=before['compaction_state_digest'],expected_files=source['expected_files'])
    pending=fields()
    # A single config replacement failure after catalog creation rolls both back.
    if not legacy:
        fault="""
real_replace = os.replace
def fail_config_once(source, destination):
    if str(destination) == os.environ['FIXTURE_CONFIG'] and not getattr(fail_config_once, 'failed', False):
        fail_config_once.failed = True
        raise OSError('fixture failure')
    return real_replace(source, destination)
os.replace = fail_config_once
"""
        failing=agent.replace('try:\n    raw = sys.stdin.buffer.read',fault+'\ntry:\n    raw = sys.stdin.buffer.read')
        os.environ['FIXTURE_CONFIG']=str(config)
        assert not call('apply',code=failing,**pending)['ok']
        assert not config.exists() and not catalog.exists() and not auth.exists()
        pending=fields()
    applied=call('apply',**pending);assert applied['ok'];baseline=applied['data']['original_backup_id']
    assert json.loads(catalog.read_bytes())['models'][0]['auto_compact_token_limit']==60000
    assert catalog.stat().st_mode&0o777==0o600
    before=call('read')['data'];assert base64.b64decode(before['compaction_base'])==(gateway+original if legacy else b'')
    pending=fields(65000)
    unchanged=config.read_bytes();saved_catalog=catalog.read_bytes()
    catalog.write_bytes(b'changed after preview')
    assert not call('apply',**pending)['ok'] and config.read_bytes()==unchanged
    catalog.write_bytes(saved_catalog)
    cache=codex/'models_cache.json';cache.write_text(json.dumps(dict(models=[dict(slug='fixture',context_window=100000)],access_token='must-not-be-exported')))
    assert not call('apply',**pending)['ok'] and config.read_bytes()==unchanged
    fetched=call('catalog')['data'];assert 'access_token' not in fetched['catalog']
    assert 'must-not-be-exported' not in json.dumps(fetched)
    cache.unlink()
    applied=call('apply',**fields(65000));assert applied['ok'] and applied['data']['original_backup_id']==baseline
    assert json.loads(catalog.read_bytes())['models'][0]['auto_compact_token_limit']==65000
    # Removing all local rules restores only the pre-compaction fields/file.
    before=call('read')['data']
    reset=gateway+(original or b'')
    result=call('apply',**dict(common,expected_digest=before['digest'],config=base64.b64encode(reset).decode(),catalog_action='restore',expected_catalog_digest=before['catalog_digest'],expected_compaction_state_digest=before['compaction_state_digest']))
    assert result['ok']
    assert catalog.read_bytes()==old_catalog if legacy else not catalog.exists()
    # Re-enable, then restore the immutable pre-gateway baseline, including absence.
    assert call('apply',**fields())['ok']
    restored=call('restore');assert restored['ok'] and restored['data']['original_backup_id']==baseline
    if legacy:
        assert config.read_bytes()==original and auth.read_bytes()==b'original-auth' and catalog.read_bytes()==old_catalog
    else:
        assert not config.exists() and not catalog.exists() and not auth.exists()
    assert (history/'keep.json').read_bytes()==b'keep conversation'
server.shutdown()
print('remote compaction apply/backup/reset/restore fixture passed')
"""#
        let task=Process();task.executableURL=URL(fileURLWithPath:"/usr/bin/env");task.arguments=["python3","-c",harness,program]
        let output=Pipe();task.standardOutput=output;task.standardError=output
        try task.run();let data=output.fileHandleForReading.readDataToEndOfFile();task.waitUntilExit()
        XCTAssertEqual(task.terminationStatus,0,String(data:data,encoding:.utf8) ?? "Fixture failed")
    }
}
