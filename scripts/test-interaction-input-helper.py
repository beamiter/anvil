#!/usr/bin/env python3
"""Pure same-connection checks and real owned-PTY hung-helper cleanup, no X11."""
import importlib.util
import io
import json
import os
from pathlib import Path
import pty
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

ROOT=Path(__file__).resolve().parent

def load(name, file):
    spec=importlib.util.spec_from_file_location(name, ROOT/file)
    module=importlib.util.module_from_spec(spec);sys.modules[name]=module;spec.loader.exec_module(module);return module
helper=load('input_helper_test','interaction-input-helper.py')
client=load('input_client_test','interaction-input-client.py')
collector=load('input_owner_test','collect-interaction-budget.py')

class FakeInput:
    def __init__(self):self.x=self;self.display=object();self.events=[]
    def keypress(self):self.events.append('key');return 10
    def drag(self,d,delta,observe=False):
        assert observe is True
        self.events.append('drag');self.last_drag_pointer=dict(start_root=[50,50],end_root=[62,50]);return 10
    def focus(self,bounds):self.events.append('focus')
    def XSync(self,display,discard):
        assert display is self.display and discard is False
        self.events.append('same_connection_sync')

class Contract(unittest.TestCase):
    def test_injection_then_same_connection_sync(self):
        for command in [dict(kind='key',name=1),dict(kind='focus',name=2,bounds=[0,0,50,50]),
                        dict(kind='drag',name=3,divider=dict(x=50,y=50,horizontal=True),delta=12)]:
            x=FakeInput();r=helper.execute(command,x)
            self.assertEqual(x.events,[command['kind'],'same_connection_sync']);self.assertGreaterEqual(r['completed_ns'],r['injected_ns'])
    def test_numeric_controls_only(self):
        for command in [dict(kind='shell',name=1),dict(kind='key',name=1,command='anything'),
                        dict(kind='focus',name=1,bounds=[True,0,10,10]),
                        dict(kind='drag',name=1,divider=dict(x=50,y=50,horizontal=True),delta=True),
                        dict(kind='focus',name=1,bounds=[0,0,2000,10])]:
            with self.assertRaises(ValueError):helper.execute(command,FakeInput())
    def test_collector_active_loop_has_no_xlib_barrier(self):
        import ast
        tree=ast.parse((ROOT/'collect-interaction-budget.py').read_text())
        main=next(n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name=='main')
        self.assertFalse(any(isinstance(n,ast.Attribute) and n.attr=='XSync' for n in ast.walk(main)))
    def test_hung_same_connection_worker_is_bounded_and_owned_cleanup_preserves_sentinel(self):
        sentinel=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'])
        owner=collector.OwnedProcesses(); app=worker=None;master,slave=pty.openpty()
        try:
            app=subprocess.Popen([sys.executable,'-c','import signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(60)'],stdin=slave,stdout=slave,stderr=slave)
            owner.bind(app)
            code='''import importlib.util,sys,time,signal,os
spec=importlib.util.spec_from_file_location('worker',sys.argv[1]);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
class Input:
 def __init__(self,pid):self.origin=(0,0);self.x=self;self.display=1
 def surface_identity(self):return dict(window_xid=17,surface_size=[1600,1000],surface_root_origin=[0,0])
 def keypress(self):return time.monotonic_ns()
 def XSync(self,*args):
  signal.signal(signal.SIGTERM,signal.SIG_IGN)
  while True:signal.pause()
 def close(self):pass
m.serve(sys.stdin.buffer,sys.stdout,Input)
'''
            worker=subprocess.Popen([sys.executable,'-I','-c',code,str(ROOT/'interaction-input-helper.py')],stdin=subprocess.PIPE,stdout=subprocess.PIPE)
            ipc=client.InputClient.__new__(client.InputClient);ipc.process=worker;ipc.buffer=b'';ipc.sequence=0;ipc.check_cancelled=lambda:None
            os.set_blocking(worker.stdout.fileno(),False)
            ipc.command('initialize',pid=app.pid);owner.refresh()
            start=time.monotonic()
            with self.assertRaises(TimeoutError):ipc.keypress()
            self.assertLess(time.monotonic()-start,7)
            self.assertIn(worker.pid,owner.handles)
            ipc.close()
            cleanup=owner.cleanup(grace=.1)
            self.assertTrue(cleanup['verified_empty'],cleanup)
            self.assertIsNone(sentinel.poll())
            app.wait(timeout=2);worker.wait(timeout=2)
        finally:
            owner.cleanup(grace=.1);owner.close()
            for p in (app,worker,sentinel):
                if p is not None:
                    if p.poll() is None:p.kill()
                    p.wait(timeout=2)
            os.close(master);os.close(slave)


class Cancellation(unittest.TestCase):
    def test_dedicated_collector_signals_escape_hung_sync_and_preserve_failed_evidence(self):
        for sig in (signal.SIGTERM,signal.SIGINT):
            with self.subTest(signal=sig), tempfile.TemporaryDirectory() as directory:
                root=Path(directory)
                sentinel=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'])
                worker_code = """import importlib.util,sys,time,signal
from pathlib import Path
spec=importlib.util.spec_from_file_location('worker',sys.argv[1]);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
class Input:
 def __init__(self,pid):self.origin=(0,0);self.x=self;self.display=1
 def surface_identity(self):return dict(window_xid=17,surface_size=[1600,1000],surface_root_origin=[0,0])
 def keypress(self):return time.monotonic_ns()
 def XSync(self,*args):
  signal.signal(signal.SIGTERM,signal.SIG_IGN)
  Path(sys.argv[2]).write_text('blocked')
  while True:signal.pause()
 def close(self):pass
m.serve(sys.stdin.buffer,sys.stdout,Input)
"""
                child_code = """import importlib.util,sys,os,pty,subprocess,json
from pathlib import Path
scripts=Path(sys.argv[1]);root=Path(sys.argv[2])
def load(name,file):
 spec=importlib.util.spec_from_file_location(name,scripts/file);m=importlib.util.module_from_spec(spec);sys.modules[name]=m;spec.loader.exec_module(m);return m
c=load('collector','collect-interaction-budget.py');i=load('inputclient','interaction-input-client.py')
evidence={'raw_capture_completed':False,'keys':[{'id':0}],'pixel_events':[{'id':0,'status':'pending'}]}
cancel=c.CancellationSignals(evidence);owner=c.OwnedProcesses();master,slave=pty.openpty();app=worker=ipc=None
reports=root/'reports';reports.mkdir()
try:
 app=subprocess.Popen([sys.executable,'-c','import signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(60)'],stdin=slave,stdout=slave,stderr=slave);owner.bind(app)
 worker=subprocess.Popen([sys.executable,'-I','-c',sys.argv[3],str(scripts/'interaction-input-helper.py'),str(root/'blocked')],stdin=subprocess.PIPE,stdout=subprocess.PIPE)
 ipc=i.InputClient.__new__(i.InputClient);ipc.process=worker;ipc.buffer=b'';ipc.sequence=0;ipc.check_cancelled=c.check_cancelled
 os.set_blocking(worker.stdout.fileno(),False);ipc.command('initialize',pid=app.pid);owner.refresh()
 evidence['input_helper_pid']=worker.pid;evidence['application_pid']=app.pid
 ipc.keypress()
except Exception as error:evidence['error']=str(error)
finally:
 cancel.begin_cleanup()
 c.finalize(evidence,root/'failed.json',app,ipc,None,owner,root,reports,False)
 cancel.restore();os.close(master);os.close(slave)
"""
                child=None
                try:
                    child=subprocess.Popen([sys.executable,'-I','-c',child_code,str(ROOT),str(root),worker_code])
                    deadline=time.monotonic()+5
                    while not (root/'blocked').exists() and time.monotonic()<deadline:time.sleep(.01)
                    self.assertTrue((root/'blocked').exists())
                    child.send_signal(sig);child.wait(timeout=12)
                    saved=json.loads((root/'failed.json').read_text())
                    self.assertFalse(saved['raw_capture_completed']);self.assertIn('cancelled',saved['error'])
                    self.assertTrue(saved['cleanup']['verified_empty'],saved['cleanup'])
                    self.assertEqual(saved['keys'],[dict(id=0)]);self.assertEqual(saved['pixel_events'][0]['status'],'pending')
                    self.assertIn(saved['input_helper_pid'],{x['pid'] for x in saved['cleanup']['owned_identities']})
                    self.assertIsNone(sentinel.poll())
                finally:
                    for process in (child,sentinel):
                        if process is not None:
                            if process.poll() is None:process.kill()
                            process.wait(timeout=3)

if __name__=='__main__':unittest.main()
