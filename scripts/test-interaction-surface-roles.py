#!/usr/bin/env python3
"""Pure X11 drawable-role contracts; no display, socket, or native execution."""
import contextlib
import copy
import ctypes
import importlib.util
import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch

ROOT=Path(__file__).resolve().parent

def load(name,file):
    spec=importlib.util.spec_from_file_location(name,ROOT/file);m=importlib.util.module_from_spec(spec);sys.modules[name]=m;spec.loader.exec_module(m);return m
geometry=load('surface_roles_geometry','interaction-pixel-client.py')
collector=load('surface_roles_collector','collect-interaction-budget.py')
client=load('surface_roles_client','interaction-input-client.py')

def identity(client_depth=32,root_depth=24):
    return dict(window_xid=17,client_root_xid=1,root_window_xid=1,root_geometry_root_xid=1,
                client_depth=client_depth,root_depth=root_depth,root_size=[1600,1000],
                surface_size=[1600,1000],surface_root_origin=[0,0])

class SurfaceRoles(unittest.TestCase):
    def test_root24_client24_or32_are_valid_geometry_roles(self):
        for depth in (24,32):geometry.validate_surface_identity(identity(depth))
    def test_root_and_client_identity_mismatches_rejected(self):
        for field,value in [('client_root_xid',2),('root_geometry_root_xid',2),('window_xid',1),
                            ('root_window_xid',True),('client_root_xid',None)]:
            d=identity();d[field]=value
            with self.subTest(field=field),self.assertRaises(ValueError):geometry.validate_surface_identity(d)
    def test_capture_root_depth_is_independent_and_strict(self):
        for root_depth in (16,30,32,True,None):
            with self.subTest(root_depth=root_depth),self.assertRaises(ValueError):geometry.validate_surface_identity(identity(32,root_depth))
        for client_depth in (1,8,16,30,True,None):
            with self.subTest(client_depth=client_depth),self.assertRaises(ValueError):geometry.validate_surface_identity(identity(client_depth))
    def test_missing_role_or_invalid_fixed_geometry_rejected(self):
        for key in identity():
            d=identity();del d[key]
            with self.subTest(key=key),self.assertRaises((ValueError,KeyError)):geometry.validate_surface_identity(d)
        for field,value in [('root_size',[800,600]),('root_size',[1600.0,1000]),('surface_size',[800,600]),('surface_root_origin',[True,0])]:
            d=identity();d[field]=value
            with self.subTest(field=field),self.assertRaises(ValueError):geometry.validate_surface_identity(d)
    def test_raw_geometry_is_recorded_before_translation_can_fail(self):
        calls=[]
        class GetGeometry:
            def __call__(self,display,drawable,root,x,y,width,height,border,depth):
                calls.append(drawable)
                ctypes.cast(root,ctypes.POINTER(ctypes.c_ulong))[0]=1
                ctypes.cast(width,ctypes.POINTER(ctypes.c_uint))[0]=1600
                ctypes.cast(height,ctypes.POINTER(ctypes.c_uint))[0]=1000
                ctypes.cast(depth,ctypes.POINTER(ctypes.c_uint))[0]=32 if drawable==17 else 24
                return 1
        x=collector.XInput.__new__(collector.XInput);x.x=Mock();x.x.XGetGeometry=GetGeometry();x.display=object();x.window=17;x.root=1
        x.refresh_origin=Mock(side_effect=RuntimeError('translation failure'))
        output=io.StringIO()
        with contextlib.redirect_stderr(output),self.assertRaisesRegex(RuntimeError,'translation failure'):x.surface_identity()
        record=json.loads(output.getvalue().split(' ',1)[1]);self.assertEqual(calls,[17,1])
        self.assertEqual(record['client_depth'],32);self.assertEqual(record['root_depth'],24)
        self.assertEqual(record['client_root_xid'],record['root_window_xid'])
    def test_parent_persists_observation_before_rejecting_it(self):
        d=identity(root_depth=32);seen=[];process=Mock(pid=99)
        with patch.object(client.subprocess,'Popen',return_value=process),patch.object(client.os,'set_blocking'),patch.object(client.InputClient,'command',return_value=dict(identity=d)):
            with self.assertRaisesRegex(ValueError,'captured root depth'):
                client.InputClient(50,{},lambda:None,lambda pid:None,seen.append)
        self.assertEqual(seen,[d])

if __name__=='__main__':unittest.main()
