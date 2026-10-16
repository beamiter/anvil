#!/usr/bin/env python3
"""Exact GTK API-offset sign, identity and pixel/input composition; no display."""
import copy
import importlib.util
from pathlib import Path
import sys
import unittest

ROOT=Path(__file__).resolve().parent

def load(name,file):
    spec=importlib.util.spec_from_file_location(name,ROOT/file);m=importlib.util.module_from_spec(spec);sys.modules[name]=m;spec.loader.exec_module(m);return m
geometry=load('native_geometry','interaction-pixel-client.py')
input_client=load('native_input_client','interaction-input-client.py')
helper=load('native_helper','interaction-input-helper.py')

class NativeCoordinates(unittest.TestCase):
    def frame(self,api=(5.0,5.0),root=(0,0)):
        native=dict(units='logical_pixels_at_scale_1',scale_factor=1,window_xid=17,
                    surface_size=[1600,1000],widget_size=[1590,990],native_api_offset=list(api),
                    surface_to_widget_translation=[-x for x in api])
        return dict(request=5,monotonic_ns=1,native_frame=native,window_size=[1590,990]),dict(window_xid=17,surface_size=[1600,1000],surface_root_origin=list(root))
    def test_api_sign_inverse_translation_and_root_composition(self):
        for api,root,expected in [((5,5),(0,0),[5,5]),((-3,-4),(20,30),[17,26]),((0,0),(20,30),[20,30]),((5.25,-.5),(0,1),[5.25,.5])]:
            snap,identity=self.frame(api,root)
            self.assertEqual(geometry.native_coordinate_frame(snap,identity),expected)
    def test_native_widget_sizes_cannot_be_conflated(self):
        snap,identity=self.frame();self.assertEqual(geometry.native_coordinate_frame(snap,identity),[5,5])
        identity['surface_size']=[1590,990]
        with self.assertRaises(ValueError):geometry.native_coordinate_frame(snap,identity)
    def test_missing_sign_units_scale_identity_and_nonfinite(self):
        for field,value in [('units','device_pixels'),('scale_factor',2),('scale_factor',True),('window_xid',18),
                            ('native_api_offset',[float('nan'),5]),('native_api_offset',[True,5]),
                            ('surface_to_widget_translation',[5,5]),('surface_to_widget_translation',[-65,0])]:
            snap,identity=self.frame();snap['native_frame'][field]=value
            with self.assertRaises(ValueError):geometry.native_coordinate_frame(snap,identity)
        snap,identity=self.frame();snap.pop('native_frame')
        with self.assertRaises(KeyError):geometry.native_coordinate_frame(snap,identity)
    def test_fractional_roi_rounds_inward_and_screen_bounds_hold(self):
        snap,identity=self.frame((5.25,5.75));origin=geometry.native_coordinate_frame(snap,identity)
        pane=dict(bounds=[10,20,800,600],pixel_geometry=dict(bounds=[18,28,784,584],cell_width=10,cell_height=20))
        rect,_,_=geometry.marker_rect(pane,origin)
        self.assertEqual(rect,[24,537,330,80])
        pane['bounds']=[1590,20,800,600];pane['pixel_geometry']['bounds']=[1590,20,800,600]
        with self.assertRaises(ValueError):geometry.marker_rect(pane,origin)
    def test_input_and_roi_use_identical_fresh_frame_and_window(self):
        snap,identity=self.frame()
        class Window:
            def surface_identity(self):return copy.deepcopy(identity)
        window=Window()
        client=input_client.InputClient.__new__(input_client.InputClient);client.identity=identity;client.native_frame=None
        client.command=lambda kind,**args:helper.execute(dict(kind=kind,name=1,**args),window)
        ack=client.configure(snap)
        self.assertEqual(window.origin,client.origin)
        self.assertEqual(list(client.origin),geometry.native_coordinate_frame(snap,identity))
        self.assertEqual(ack['snapshot_request'],snap['request'])
        self.assertEqual(ack['surface_identity']['window_xid'],17)
        changed=copy.deepcopy(snap);changed['native_frame']['native_api_offset']=[6,5];changed['native_frame']['surface_to_widget_translation']=[-6,-5]
        with self.assertRaises(ValueError):client.configure(changed)
    def test_helper_rejects_different_actual_window_even_if_parent_claims_match(self):
        snap,identity=self.frame()
        class Window:
            def surface_identity(self):return dict(identity,window_xid=99)
        with self.assertRaises(ValueError):helper.execute(dict(kind='configure',name=1,native_frame=snap['native_frame'],window_size=snap['window_size']),Window())
    def test_invalid_origin_and_oversized_geometry(self):
        for field,value in [('surface_root_origin',[True,0]),('surface_root_origin',[-1,0]),('window_xid',True)]:
            snap,identity=self.frame();identity[field]=value
            with self.assertRaises(ValueError):geometry.native_coordinate_frame(snap,identity)
        snap,identity=self.frame();snap['native_frame']['widget_size']=[4096,990];snap['window_size']=[4096,990]
        with self.assertRaises(ValueError):geometry.native_coordinate_frame(snap,identity)

class NarrowHandleModel(unittest.TestCase):
    def snapshot(self, horizontal=True):
        return dict(gtk_runtime_version=[4,14,5], dividers=[dict(x=796.5 if horizontal else 400,
            y=400 if horizontal else 796.5, horizontal=horizontal, position=796, paned_identity=77,
            wide_handle=False, direction='ltr', min_position=100, max_position=1400,
            paned_bounds=[0,0,1590,990],handle_bounds_local=[[796,0,1,990] if horizontal else [0,796,1590,1]])])
    def test_signed_pointer_delta_predicts_before_observation(self):
        for horizontal in (True,False):
            for delta,shift in ((12,6),(-12,-18)):
                model=geometry.expected_drag(self.snapshot(horizontal),delta,[5,5])
                self.assertEqual(model['expected_allocation_shift'],shift)
                self.assertEqual(model['predicted_position'],796+shift)
                self.assertEqual(model['grab_offset'],7)
    def test_runtime_and_handle_preconditions_fail_closed(self):
        for field,value in [('gtk_runtime_version',[4,16,0]),('gtk_runtime_version',[4,14,True])]:
            snap=self.snapshot();snap[field]=value
            with self.assertRaises(ValueError):geometry.expected_drag(snap,12,[5,5])
        for field,value in [('wide_handle',True),('wide_handle',0),('direction','rtl'),('direction',None),
                            ('min_position',803),('max_position',802),('paned_identity',True),
                            ('handle_bounds_local',[]),('handle_bounds_local',[[796,0,1,990],[796,0,1,990]]),
                            ('handle_bounds_local',[[790,0,1,990]]),('x',700)]:
            snap=self.snapshot();snap['dividers'][0][field]=value
            with self.subTest(field=field,value=value),self.assertRaises(ValueError):geometry.expected_drag(snap,12,[5,5])
    def test_rounding_and_measured_origin_are_used(self):
        snap=self.snapshot();d=snap['dividers'][0];d['paned_bounds'][0]=20;d['x']+=20
        model=geometry.expected_drag(snap,12,[5.25,5])
        self.assertEqual(model['pointer_start_root'],[822,405])
        self.assertEqual(model['pointer_start_local'][0],796.75)
        self.assertEqual(model['expected_allocation_shift'],6)
    def test_pointer_observation_brackets_press_motion_release(self):
        collector=load('coordinate_collector','collect-interaction-budget.py')
        x=collector.XInput.__new__(collector.XInput);events=[];x.display=object()
        class X:
            def XFlush(self,display):events.append('flush')
        x.x=X();x.move=lambda a,b:events.append(('move',a,b));x.button=lambda pressed:events.append(('button',pressed))
        def pointer():
            events.append('query');return [801,405] if events.count('query')==1 else [813,405]
        x.pointer_root=pointer
        x.drag(dict(x=796,y=400,horizontal=True),12,observe=True)
        self.assertEqual(events,[('move',796,400),'query',('button',True),'flush',('move',808,400),'query',('button',False),'flush'])
        self.assertEqual(x.last_drag_pointer,dict(start_root=[801,405],end_root=[813,405]))
        client=input_client.InputClient.__new__(input_client.InputClient);client.origin=(5,5)
        client.command=lambda *a,**k:dict(injected_ns=1,pointer_observation=dict(start_root=[0,0],end_root=[12,0]))
        with self.assertRaises(ValueError):client.drag(dict(x=796,y=400,horizontal=True),12)

    def test_after_geometry_cannot_define_expected_motion(self):
        snap=self.snapshot();model=geometry.expected_drag(snap,12,[5,5]);snap['dividers'][0]['position']+=6
        with self.assertRaises(ValueError):geometry.expected_drag(snap,12,[5,5])
        self.assertEqual(model['predicted_position'],802)

if __name__=='__main__':unittest.main()
