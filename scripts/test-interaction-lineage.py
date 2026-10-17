#!/usr/bin/env python3
"""Pure synthetic evidence only. Never imports/starts a GUI or process fixture."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import unittest
spec=importlib.util.spec_from_file_location('lineage',Path(__file__).with_name('normalize-interaction-lineage.py'))
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)


def fixture(n=8):
    start=10_000_000_000; stop=start+60_000_000_000; idle=stop+10_000_000_000
    tabs=[]
    for i in range(2 if n==32 else 1):
        tabs.append(dict(tab_id=i,panes=[dict(pid=100+i*16+j,block=True,mapped=i==0,columns=20,rows=8)
                                       for j in range(16 if n==32 else n)]))
    setup=dict(fixture_program_matches=True,tabs=tabs,active_tab_index=0)
    keys=[]; snapshots=[]; markers=[]; seqs={}; drags=[]
    for i in range(300):
        scheduled=start+int((.1+i*(60-.2)/300)*1e9); injected=scheduled+1000
        tab=int(((scheduled-start)/1e9)/10)%len(tabs)
        snap=copy.deepcopy(setup);snap.update(request=i,monotonic_ns=scheduled,active_tab_index=tab)
        for t in snap['tabs']:
            for p in t['panes']:p['mapped']=t['tab_id']==tab
        snapshots.append(snap);pid=tabs[tab]['panes'][0]['pid'];seqs[pid]=seqs.get(pid,0)+1
        keys.append(dict(id=i,pid=pid,sequence=seqs[pid],scheduled_ns=scheduled,injected_ns=injected,
                         active_tab_id=tab,scheduled_tab_id=tab,target_snapshot_request=i,target_snapshot_ns=scheduled))
        markers.append([pid,seqs[pid],injected+1_000_000])
        if n>1:drags.append(dict(id=i,injected_ns=injected+2_000_000,delta=12 if i%2==0 else -12))
    raw=dict(schema='raw-native-block-v1',smoke=False,budget_acceptance=False,binary_sha256='c'*64,
             raw_capture_completed=True,evidence_write_succeeded=True,cancellation_count=0,harvest_errors=[],
             finalization_errors=[],panes=n,warmup_started_ns=1_000_000_000,warmup_ended_ns=6_000_000_000,
             active_started_ns=start,active_stopped_ns=stop,idle_started_ns=idle,idle_ended_ns=idle+1_200_000_000_000,
             setup_snapshot=setup,snapshots=snapshots,keys=keys,drags=drags,
             cleanup=dict(verified_empty=True,remaining_pids=[],errors=[],owned_identities=[dict(pid=p['pid'],start_ticks=1) for t in tabs for p in t['panes']]),
             workload=dict(kind='full_budget',key_count=300,active_seconds=60,tab_switch_seconds=10,tab_ids=list(range(len(tabs)))),
             raw_probe=dict(metric='after_paint_proxy_not_display',overflow=False,timer_fallback=False,vte_spawn_count=0,
                            dispatch_ids=[10,20,30],consumer_dispatch_ids=[10,10,30],dispatch_ms=[.1,.2,.3],
                            consumer_ms=[.01,.02,.03],painted_markers=markers))
    initial = []
    for total in range(1,n+1):
        snap=copy.deepcopy(setup);snap['tabs']=[]
        remaining=total
        for tab in tabs:
            if not remaining: break
            take=min(len(tab['panes']),remaining)
            piece=copy.deepcopy(tab);piece['panes']=piece['panes'][:take]
            snap['tabs'].append(piece);remaining-=take
        snap.update(request=total-1,monotonic_ns=500_000_000+total*1_000_000,
                    active_tab_index=len(snap['tabs'])-1)
        for t in snap['tabs']:
            for p in t['panes']:p['mapped']=t['tab_id']==snap['active_tab_index']
        initial.append(snap)
    for snap in snapshots:snap['request']+=n
    for key in keys:key['target_snapshot_request']+=n
    raw['setup_snapshot']=copy.deepcopy(snapshots[-1])
    finish=copy.deepcopy(snapshots[-1]);finish.update(raw['raw_probe'])
    finish.update(request=n+300,monotonic_ns=raw['idle_ended_ns']+1)
    raw['raw_probe']=finish
    raw['snapshots']=initial+snapshots+[copy.deepcopy(finish)]
    raw.update(lifecycle_started_ns=100_000_000,cleanup_started_ns=raw['idle_ended_ns']+2,
               cleanup_ended_ns=raw['idle_ended_ns']+100_000_000)
    raw['fixtures']=[dict(pid=p['pid'],input_count=seqs.get(p['pid'],0),marker_sequence=seqs.get(p['pid'],0))
                     for t in tabs for p in t['panes']]
    raw['fixture_reports']={str(p['pid']):copy.deepcopy(p) for p in raw['fixtures']}
    env={k:'fixed'  for k in m.budget.ENV_KEYS};env.update({k:'b'*64 for k in m.budget.ENV_DIGEST_KEYS})
    env.update(build_profile='release',renderer='cairo',backend='x11')
    metadata=dict(schema='a04-lineage-binding-v1',source_sha='a'*40,source_tree='d'*40,binary_sha256='c'*64,
                  pair_id='synthetic-pair',role='candidate',repetition=1,environment=env)
    return raw,metadata


def run(raw,meta):
    data=json.dumps(raw,allow_nan=True).encode();meta=copy.deepcopy(meta)
    meta['raw_sha256']=hashlib.sha256(data).hexdigest()
    return m.stage(data,meta)


class Contract(unittest.TestCase):
    def test_each_topology_and_nonacceptance(self):
        for n in (1,8,32):
            r,b=fixture(n); before=copy.deepcopy(r);out=run(r,b)
            self.assertEqual(r,before);self.assertEqual(len(out['keys']),300)
            self.assertEqual(out['keys'][0]['input_to_after_paint_ms'],1)
            self.assertEqual(out['keys'][0]['lateness_ns'],1000)
            self.assertFalse(out['budget_acceptance']);self.assertFalse(out['comparator_compatible'])
            self.assertFalse(out['physical_scanout_proven']);self.assertFalse(out['drag_geometry_linked'])
            self.assertTrue(out['missing_gates'])
            with self.assertRaises(ValueError):m.budget.validate(out)
            with self.assertRaises(ValueError):m.for_budget(out)
    def test_required_top_level_fields(self):
        r,b=fixture()
        for key in r:
            bad=copy.deepcopy(r);del bad[key]
            with self.subTest(key=key),self.assertRaises(ValueError):run(bad,b)
    def test_required_binding_fields(self):
        r,b=fixture()
        for key in b:
            bad=copy.deepcopy(b);del bad[key]
            with self.subTest(key=key),self.assertRaises(ValueError):run(r,bad)
        for key in b['environment']:
            bad=copy.deepcopy(b);del bad['environment'][key]
            with self.subTest(env=key),self.assertRaises(ValueError):run(r,bad)
    def test_required_key_fields(self):
        r,b=fixture()
        for key in r['keys'][0]:
            bad=copy.deepcopy(r);del bad['keys'][0][key]
            with self.subTest(key=key),self.assertRaises(ValueError):run(bad,b)
    def test_windows(self):
        for key,value in [('warmup_ended_ns',5_999_999_999),('active_stopped_ns',69_999_999_999),
                          ('idle_started_ns',79_999_999_999),('idle_ended_ns',1_279_999_999_999),
                          ('active_started_ns',True),('warmup_started_ns',0)]:
            r,b=fixture();r[key]=value
            with self.subTest(key=key),self.assertRaises(ValueError):run(r,b)
    def test_failed_capture(self):
        for key,value in [('smoke',True),('raw_capture_completed',False),('budget_acceptance',True),
                          ('evidence_write_succeeded',False),('cancellation_count',1),
                          ('harvest_errors',['bad']),('finalization_errors',['bad']),('binary_sha256','e'*64)]:
            r,b=fixture();r[key]=value
            with self.subTest(key=key),self.assertRaises(ValueError):run(r,b)
    def test_keys_negative(self):
        for key,value in [('id',True),('id',1),('pid',999),('sequence',2),('sequence',True),
                          ('scheduled_ns',10_100_000_001),('injected_ns',10_099_999_999),
                          ('injected_ns',70_000_000_000),('active_tab_id',9),('scheduled_tab_id',9),
                          ('target_snapshot_request',999),('target_snapshot_ns',99)]:
            r,b=fixture();r['keys'][0][key]=value
            with self.subTest(key=key,value=value),self.assertRaises(ValueError):run(r,b)
    def test_markers_negative(self):
        mutations=[lambda a:a.pop(),lambda a:a.append(a[0]),lambda a:a.__setitem__(0,[999,1,10_102_000_000]),
                   lambda a:a.__setitem__(0,[100,1,10_100_000_000]),lambda a:a.__setitem__(0,[100,1,80_000_000_001]),
                   lambda a:a.__setitem__(0,[True,1,10_102_000_000]),lambda a:a.reverse()]
        for mutation in mutations:
            r,b=fixture();mutation(r['raw_probe']['painted_markers']);r['snapshots'][-1]=copy.deepcopy(r['raw_probe'])
            with self.assertRaises(ValueError):run(r,b)
    def test_callbacks_negative(self):
        for key,value in [('dispatch_ids',[10,10,30]),('dispatch_ids',[True,20,30]),
                          ('consumer_dispatch_ids',[40,10,30]),('dispatch_ms',[.1]),('dispatch_ms',[float('nan'),.1,.2]),
                          ('consumer_ms',[-1,.1,.2]),('overflow',True),('timer_fallback',True),('vte_spawn_count',True)]:
            r,b=fixture();r['raw_probe'][key]=value
            with self.subTest(key=key),self.assertRaises(ValueError):run(r,b)
    def test_cleanup_negative(self):
        for key,value in [('verified_empty',False),('remaining_pids',[100]),('errors',['failure']),('owned_identities',[])]:
            r,b=fixture();r['cleanup'][key]=value
            with self.subTest(key=key),self.assertRaises(ValueError):run(r,b)
    def test_topology_and_snapshots_negative(self):
        changes=[lambda r:r['snapshots'][9].update(request=8),
                 lambda r:r['snapshots'][9].update(monotonic_ns=1),
                 lambda r:r['snapshots'][9]['tabs'][0]['panes'][0].update(pid=200),
                 lambda r:r['setup_snapshot']['tabs'][0]['panes'][0].update(block=False),
                 lambda r:r['setup_snapshot']['tabs'][0]['panes'][0].update(columns=15)]
        for change in changes:
            r,b=fixture();change(r)
            with self.assertRaises(ValueError):run(r,b)
    def test_drag_negative_and_not_inferred(self):
        for key,value in [('id',1),('delta',13),('injected_ns',1)]:
            r,b=fixture();r['drags'][0][key]=value
            with self.subTest(key=key),self.assertRaises(ValueError):run(r,b)
        r,b=fixture();r['drags'].pop()
        with self.assertRaises(ValueError):run(r,b)
        r,b=fixture(1);r['drags']=[dict(id=0,injected_ns=1,delta=12)]
        with self.assertRaises(ValueError):run(r,b)
    def test_binding_bytes_and_strict_json(self):
        r,b=fixture();data=json.dumps(r).encode();b['raw_sha256']='f'*64
        with self.assertRaises(ValueError):m.stage(data,b)
        for data in (b'{"x":1,"x":2}',b'{"x":NaN}',b'{"x":Infinity}',b'',b'0'*(m.MAX_BYTES+1)):
            with self.assertRaises(ValueError):m.strict_json(data)
    def test_for_budget_never_trusts_supplied_flags(self):
        with self.assertRaises(ValueError):m.for_budget(dict(idle_source_audit_passed=True,pixel_oracle_passed=True,budget_acceptance=True))

    def test_natural_lifecycle_preserves_all_responses(self):
        for n in (1,8,32):
            r,b=fixture(n);out=run(r,b)
            self.assertEqual([v['snapshot'] for v in out['control_responses']],r['snapshots'])
            self.assertEqual(out['control_responses'][0]['phase'],'setup')
            self.assertEqual(out['control_responses'][-1]['phase'],'finalization')
            self.assertGreater(r['raw_probe']['monotonic_ns'],r['idle_ended_ns'])
    def test_setup_identity_and_finish_negatives(self):
        mutations=[lambda r:r['snapshots'][1]['tabs'][0]['panes'].pop(0),
                   lambda r:r['snapshots'][0]['tabs'][0]['panes'][0].update(pid=999),
                   lambda r:r['snapshots'][-1].update(request=999),
                   lambda r:r['snapshots'].append(copy.deepcopy(r['snapshots'][-1])),
                   lambda r:r['snapshots'].pop(),
                   lambda r:r['snapshots'][0].update(monotonic_ns=1),
                   lambda r:r['setup_snapshot'].update(request=999)]
        for mutation in mutations:
            r,b=fixture();mutation(r)
            with self.assertRaises(ValueError):run(r,b)
    def test_fixture_identity_and_counts(self):
        mutations=[lambda r:r.update(fixtures=[dict(pid=99999)]),
                   lambda r:r['fixtures'].pop(),
                   lambda r:r['fixtures'].__setitem__(1,copy.deepcopy(r['fixtures'][0])),
                   lambda r:r['fixtures'][0].update(input_count=299),
                   lambda r:r['fixture_reports'].pop('100'),
                   lambda r:r['fixture_reports']['100'].update(pid=999),
                   lambda r:r['fixture_reports']['100'].update(marker_sequence=299)]
        for mutation in mutations:
            r,b=fixture();mutation(r)
            with self.assertRaises(ValueError):run(r,b)
    def test_nonfinite_envelope_direct_api(self):
        r,b=fixture();b['environment']['scale']=float('inf')
        with self.assertRaises(ValueError):run(r,b)

if __name__=='__main__':unittest.main()
