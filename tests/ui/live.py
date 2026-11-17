"""Isolated real-service UI test. Run on test host; never point it at production."""
import json, os, pathlib, socket, subprocess, sys, tempfile, time, urllib.request
app=json.loads((pathlib.Path(__file__).parent/'project.json').read_text())['name']
binary=str(pathlib.Path(os.environ['UI_BINARY']).resolve())
with tempfile.TemporaryDirectory(prefix=app+'-ui-') as tmp:
    root=pathlib.Path(tmp)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    if app=='apwatch':
        (root/'config.json').write_text(json.dumps({'ap_host':'127.0.0.1','password':'fixture-password','unlock':'unused-fixture','paused_since':int(time.time()),'leases_path':''}))
        args=[binary,'--listen',f'127.0.0.1:{port}','--data-dir',tmp]
    else:
        (root/'demo').mkdir()
        (root/'demo/app.toml').write_text('name="demo"\ntitle="隔离测试程序"\nexe="run"\nautostart=false\nstop_timeout_secs=2\n')
        script=root/'demo-script'; script.write_text('#!/bin/sh\necho "fixture ready"\nexec sleep 300\n')
        subprocess.run([binary,'--root',tmp,'install','demo',str(script)],check=True,stdout=subprocess.DEVNULL)
        args=[binary,'--root',tmp,'serve','--listen',f'127.0.0.1:{port}']
    env=dict(os.environ,TOKIO_WORKER_THREADS='2',UI_URL=f'http://127.0.0.1:{port}')
    with (root/'service.log').open('w') as log:
        proc=subprocess.Popen(args,env=env,stdout=log,stderr=log)
        try:
            for _ in range(100):
                if proc.poll() is not None: raise RuntimeError('Service exited: '+(root/'service.log').read_text())
                try:
                    with urllib.request.urlopen(env['UI_URL'],timeout=1) as response: served=response.read()
                    expected=(pathlib.Path(__file__).resolve().parents[2]/'assets/index.html').read_bytes()
                    assert served==expected,'embedded HTML differs from tested source'
                    break
                except OSError: time.sleep(.1)
            else: raise RuntimeError('Service not ready')
            if app=='apwatch':
                with urllib.request.urlopen(env['UI_URL']+'/classic',timeout=1) as response:
                    assert response.url==env['UI_URL']+'/classic/'
                    assert response.headers['Cache-Control']=='no-cache'
                    expected=(pathlib.Path(__file__).resolve().parents[2]/'assets/classic/index.html').read_bytes()
                    assert response.read()==expected,'embedded classic HTML differs from tested source'
            subprocess.run(['node',str(pathlib.Path(__file__).with_name('live.cjs'))],env=env,check=True,timeout=60)
            if app=='apwatch':
                settings=json.loads((root/'config.json').read_text())
                assert settings['paused_since'] and settings['ap_host']=='127.0.0.1'
                assert settings['fast_secs']==40
            print(app+': real-service UI + embedded source identity PASS')
        finally:
            proc.terminate()
            try: proc.wait(timeout=10)
            except subprocess.TimeoutExpired: proc.kill(); proc.wait()
