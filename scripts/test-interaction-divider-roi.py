#!/usr/bin/env python3
"""Pre-input allocation-only divider sampling, without a display or pixel search."""
import copy
import importlib.util
from pathlib import Path
import sys
import unittest

spec=importlib.util.spec_from_file_location('divider_roi',Path(__file__).with_name('interaction-pixel-client.py'))
g=importlib.util.module_from_spec(spec);sys.modules[spec.name]=g;spec.loader.exec_module(g)

def snapshot(total=8):
    rows=2 if total==8 else 4;tabs=[]
    for tab in range(1 if total==8 else 2):
        panes=[]
        for row in range(rows):
            for col in range(4):
                x=col*400;y=40+row*220;header=10+row*7
                panes.append(dict(pid=100+tab*16+row*4+col,pane_id=row*4+col,mapped=tab==0,
                    bounds=[x,y,399,219],pixel_geometry=dict(bounds=[x+11,y+header,379,219-header],cell_width=10,cell_height=21)))
        tabs.append(dict(tab_id=tab,panes=panes))
    return dict(native_frame=dict(scale_factor=1),active_tab_index=0,tabs=tabs,
        dividers=[dict(x=799.5,y=480,horizontal=True,paned_identity=77,paned_bounds=[0,40,1600,927],handle_bounds_local=[[799,0,9,927]])])

class DividerROI(unittest.TestCase):
    def test_eight_and_32_total_nested_headers_are_excluded(self):
        for total in (8,32):
            s=snapshot(total);p=g.divider_sampling_plan(s,[0,0]);self.assertEqual(p['rect'],[768,130,64,48])
            self.assertEqual(len(p['candidate_intervals']),2 if total==8 else 4)
            for side in p['adjacent_content']:
                self.assertTrue(side['excluded_noncontent_intervals'])
            self.assertEqual(p['chosen']['left_pid'],101);self.assertEqual(p['chosen']['right_pid'],102)
    def test_selection_is_deterministic_and_ignores_pane_order(self):
        s=snapshot();a=g.divider_sampling_plan(s,[0,0]);s['tabs'][0]['panes'].reverse();b=g.divider_sampling_plan(s,[0,0])
        self.assertEqual(a,b)
    def test_exact_minimum_span_and_short_span_rejection(self):
        s=snapshot()
        for pane in s['tabs'][0]['panes']:pane['pixel_geometry']['bounds'][3]=48
        self.assertEqual(g.divider_sampling_plan(s,[0,0])['rect'][3],48)
        for pane in s['tabs'][0]['panes']:pane['pixel_geometry']['bounds'][3]=47
        with self.assertRaisesRegex(ValueError,'no adequate'):g.divider_sampling_plan(s,[0,0])
    def test_no_common_content_span_is_rejected(self):
        s=snapshot()
        for pane in s['tabs'][0]['panes']:
            if pane['pane_id']%4==1:pane['pixel_geometry']['bounds'][1]=pane['bounds'][1];pane['pixel_geometry']['bounds'][3]=50
            if pane['pane_id']%4==2:pane['pixel_geometry']['bounds'][1]=pane['bounds'][1]+150;pane['pixel_geometry']['bounds'][3]=50
        with self.assertRaisesRegex(ValueError,'no adequate'):g.divider_sampling_plan(s,[0,0])
    def test_after_move_checks_same_pane_content_without_rechoosing(self):
        s=snapshot();plan=g.divider_sampling_plan(s,[0,0]);after=copy.deepcopy(s)
        after['dividers'][0]['x']+=6
        for pane in after['tabs'][0]['panes']:
            col=pane['pane_id']%4
            if col==1:pane['bounds'][2]+=6;pane['pixel_geometry']['bounds'][2]+=6
            if col==2:
                for b in (pane['bounds'],pane['pixel_geometry']['bounds']):b[0]+=6;b[2]-=6
        self.assertEqual(g.verify_divider_sampling_after(plan,after,[0,0])['covered_interval'],[130,178])
        after['tabs'][0]['panes'][1]['pixel_geometry']['bounds'][1]+=150
        with self.assertRaises(ValueError):g.verify_divider_sampling_after(plan,after,[0,0])
    def test_wrong_identity_bounds_and_scale_fail_closed(self):
        for scale in (0,2,True,None):
            s=snapshot();s['native_frame']['scale_factor']=scale
            with self.assertRaises(ValueError):g.divider_sampling_plan(s,[0,0])
        for origin in ([-800,0],[1000,0],[0,1000],[float('nan'),0]):
            with self.assertRaises(ValueError):g.divider_sampling_plan(snapshot(),origin)
        s=snapshot();s['dividers'][0]['horizontal']=False
        with self.assertRaises(ValueError):g.divider_sampling_plan(s,[0,0])
        s=snapshot();s['tabs'][0]['panes'][1]['pid']=True
        with self.assertRaises(ValueError):g.divider_sampling_plan(s,[0,0])
    def test_unmapped_other_tab_cannot_supply_eligible_content(self):
        s=snapshot(32)
        for pane in s['tabs'][0]['panes']:pane['pixel_geometry']['bounds'][3]=20
        with self.assertRaises(ValueError):g.divider_sampling_plan(s,[0,0])
    def test_content_is_clipped_to_actual_pane_allocation(self):
        s=snapshot()
        for pane in s['tabs'][0]['panes']:pane['pixel_geometry']['bounds'][3]=1000
        plan=g.divider_sampling_plan(s,[0,0])
        self.assertLessEqual(plan['chosen']['interval'][1],259)
    def test_handle_span_and_adjacent_width_must_cover_fixed_roi(self):
        s=snapshot();s['dividers'][0]['handle_bounds_local'][0][3]=47
        with self.assertRaises(ValueError):g.divider_sampling_plan(s,[0,0])
        s=snapshot()
        for pane in s['tabs'][0]['panes']:
            if pane['pane_id']%4==1:
                pane['bounds'][0]=789;pane['bounds'][2]=10;pane['pixel_geometry']['bounds'][0]=790;pane['pixel_geometry']['bounds'][2]=8
            if pane['pane_id']%4==2:pane['bounds'][2]=10;pane['pixel_geometry']['bounds'][0]=801;pane['pixel_geometry']['bounds'][2]=8
        with self.assertRaises(ValueError):g.divider_sampling_plan(s,[0,0])
    def test_second_16_pane_tab_uses_its_own_content_identities(self):
        s=snapshot(32);s['active_tab_index']=1
        for index,tab in enumerate(s['tabs']):
            for pane in tab['panes']:pane['mapped']=index==1
        plan=g.divider_sampling_plan(s,[0,0]);self.assertEqual(plan['tab_id'],1)
        self.assertEqual(plan['chosen']['left_pid'],117);self.assertEqual(plan['chosen']['right_pid'],118)

    def test_no_pixel_data_or_mutation_is_used(self):
        s=snapshot();before=copy.deepcopy(s);p=g.divider_sampling_plan(s,[0,0]);self.assertEqual(s,before)
        self.assertEqual(p,g.divider_sampling_plan(copy.deepcopy(s),[0,0]))

if __name__=='__main__':unittest.main()
