#!/usr/bin/env python3
"""Private process-pipe client and bounded geometry for the short pixel gate."""
import json
import math
import os
from pathlib import Path
import select
import subprocess
import sys
import time


def finite(value):
    if type(value) not in (int, float) or not math.isfinite(value):
        raise ValueError('geometry must be finite numeric values')
    return value


def coordinates(values, count):
    if not isinstance(values, (list, tuple)) or len(values) != count:
        raise ValueError('invalid geometry dimensions')
    return [finite(value) for value in values]


def active_tab(snapshot):
    index = snapshot['active_tab_index']; tabs = snapshot['tabs']
    if type(index) is not int or not 0 <= index < len(tabs):
        raise ValueError('invalid active tab index')
    return tabs[index]


def validate_surface_identity(identity):
    """Client geometry is not captured RGB: permit 24/32 clients on a 24-depth root."""
    if type(identity) is not dict: raise ValueError('missing observed X11 surface identity')
    for field in ('window_xid','client_root_xid','root_window_xid','root_geometry_root_xid'):
        if type(identity.get(field)) is not int or not 0 < identity[field] <= 0xffffffff:
            raise ValueError('invalid observed X11 drawable identity')
    if identity['client_root_xid'] != identity['root_window_xid'] or identity['root_geometry_root_xid'] != identity['root_window_xid']:
        raise ValueError('client/root drawable identity mismatch')
    if identity['window_xid'] == identity['root_window_xid']:
        raise ValueError('owned application cannot be the root drawable')
    if type(identity.get('root_depth')) is not int or identity['root_depth'] != 24:
        raise ValueError('unsupported captured root depth')
    if type(identity.get('client_depth')) is not int or identity['client_depth'] not in (24,32):
        raise ValueError('unsupported input-only client depth')
    if type(identity.get('root_size')) is not list or any(type(v) is not int for v in identity['root_size']) or identity['root_size'] != [1600,1000]:
        raise ValueError('unsupported isolated root geometry')
    if type(identity.get('surface_size')) is not list or any(type(v) is not int for v in identity['surface_size']) or identity['surface_size'] != [1600,1000]:
        raise ValueError('client has not reached fixed geometry')
    origin = coordinates(identity.get('surface_root_origin'), 2)
    if any(type(v) is not int or v < 0 for v in origin): raise ValueError('invalid client root origin')


def native_coordinate_frame(snapshot, identity):
    """Widget→surface is inverse of the EXPLICIT signed surface→widget vector."""
    if type(identity) is not dict or type(identity.get('window_xid')) is not int or not 0 < identity['window_xid'] <= 0xffffffff:
        raise ValueError('invalid owned native window identity')
    validate_surface_identity(identity)
    frame = snapshot['native_frame']
    if type(frame) is not dict or frame.get('units') != 'logical_pixels_at_scale_1' or type(frame.get('scale_factor')) is not int or frame['scale_factor'] != 1:
        raise ValueError('missing or unsupported native coordinate frame')
    xid = frame['window_xid']
    if type(xid) is not int or xid <= 0 or xid != identity['window_xid']:
        raise ValueError('native frame belongs to a different owned X11 window')
    for field in ('surface_size', 'widget_size'):
        values = frame[field]
        if type(values) is not list or len(values) != 2 or any(type(v) is not int or not 0 < v <= 4096 for v in values):
            raise ValueError('invalid native/widget dimensions')
    if frame['surface_size'] != identity['surface_size'] or frame['surface_size'] != [1600, 1000] or frame['widget_size'] != snapshot['window_size']:
        raise ValueError('native/widget size identity mismatch')
    api = coordinates(frame['native_api_offset'], 2)
    translation = coordinates(frame['surface_to_widget_translation'], 2)
    if any(abs(v) > 64 for v in api + translation) or translation != [-v for v in api]:
        raise ValueError('invalid signed native translation')
    root = coordinates(identity['surface_root_origin'], 2)
    if any(type(v) is not int or v < 0 for v in identity['surface_root_origin']):
        raise ValueError('invalid X11 surface root origin')
    if any(frame['widget_size'][i] > frame['surface_size'][i] + 2 * abs(api[i]) for i in (0,1)):
        raise ValueError('widget dimensions incompatible with native frame')
    return [root[i] - translation[i] for i in (0,1)]


def marker_rect(pane, origin):
    geometry = pane['pixel_geometry']; cell_width = geometry['cell_width']; cell_height = geometry['cell_height']
    if type(cell_width) is not int or type(cell_height) is not int or not (1 <= cell_width <= 20 and 1 <= cell_height <= 64):
        raise ValueError('unsupported fixed-font cell geometry')
    origin = coordinates(origin, 2)
    a = coordinates(pane['bounds'], 4); b = coordinates(geometry['bounds'], 4)
    if min(a[2:] + b[2:]) <= 0: raise ValueError('nonpositive geometry')
    left = max(a[0], b[0]); top = max(a[1], b[1]); right = min(a[0] + a[2], b[0] + b[2]); bottom = min(a[1] + a[3], b[1] + b[3])
    left = math.ceil(origin[0] + left); right = math.floor(origin[0] + right)
    top = math.ceil(origin[1] + top); bottom = math.floor(origin[1] + bottom)
    width = min(right - left, 33 * cell_width)
    height = min(bottom - top, 4 * cell_height)
    if width < 31 * cell_width or height < cell_height * 2:
        raise ValueError('visible pane cannot retain complete pixel barcode')
    rect = [left, bottom - height, width, height]
    if rect[0] < 0 or rect[1] < 0 or rect[0] + width > 1600 or rect[1] + height > 1000:
        raise ValueError('marker ROI outside isolated screen')
    return rect, cell_width, cell_height


def divider_rect(divider, origin):
    horizontal = divider['horizontal']
    if type(horizontal) is not bool: raise ValueError('invalid divider orientation')
    origin = coordinates(origin, 2)
    finite(divider['x']); finite(divider['y'])
    width, height = (64, 48) if horizontal else (48, 64)
    rect = [round(origin[0] + divider['x']) - width // 2,
            round(origin[1] + divider['y']) - height // 2, width, height]
    if rect[0] < 0 or rect[1] < 0 or rect[0] + width > 1600 or rect[1] + height > 1000:
        raise ValueError('divider ROI outside isolated screen')
    return rect, 32


def divider_sampling_plan(snapshot, origin):
    """Pre-input, allocation-only ROI choice. Never inspects captured RGB."""
    frame = snapshot.get('native_frame', {})
    if type(frame.get('scale_factor')) is not int or frame['scale_factor'] != 1:
        raise ValueError('divider sampling requires measured scale one')
    divider = snapshot['dividers'][0]
    if divider.get('horizontal') is not True:
        raise ValueError('header-free horizontal-separator sampling unsupported')
    origin = coordinates(origin, 2)
    center = origin[0] + finite(divider['x'])
    handles=divider['handle_bounds_local']
    if type(handles) is not list or len(handles)!=1: raise ValueError('missing unique measured handle')
    handle=coordinates(handles[0],4);paned=coordinates(divider['paned_bounds'],4)
    if min(handle[2:]+paned[2:])<=0: raise ValueError('invalid measured handle extent')
    handle_interval=[math.ceil(origin[1]+paned[1]+handle[1]),math.floor(origin[1]+paned[1]+handle[1]+handle[3])]
    roi_left,roi_right=round(center)-32,round(center)+32
    panes = []
    for pane in active_tab(snapshot)['panes']:
        if pane.get('mapped') is not True: continue
        if type(pane.get('pane_id')) is not int or pane['pane_id']<0 or type(pane.get('pid')) is not int or pane['pid']<=0:
            raise ValueError('invalid adjacent pane identity')
        bounds = coordinates(pane['bounds'], 4)
        terminal = coordinates(pane['pixel_geometry']['bounds'], 4)
        if min(bounds[2:] + terminal[2:]) <= 0: raise ValueError('nonpositive adjacent content geometry')
        left, top = max(bounds[0],terminal[0]), max(bounds[1],terminal[1])
        right, bottom = min(bounds[0]+bounds[2],terminal[0]+terminal[2]), min(bounds[1]+bounds[3],terminal[1]+terminal[3])
        if right <= left or bottom <= top: raise ValueError('empty terminal-content intersection')
        cross = [math.ceil(origin[1]+top),math.floor(origin[1]+bottom)]
        pane_cross = [math.ceil(origin[1]+bounds[1]),math.floor(origin[1]+bounds[1]+bounds[3])]
        panes.append(dict(pane_id=pane['pane_id'],pid=pane['pid'],
            left=origin[0]+bounds[0],right=origin[0]+bounds[0]+bounds[2],
            content_interval=cross,pane_interval=pane_cross,
            excluded_noncontent_intervals=[span for span in ([pane_cross[0],cross[0]],[cross[1],pane_cross[1]]) if span[1]>span[0]]))
    panes.sort(key=lambda p:(p['pane_id'],p['pid']))
    left_candidates = [p for p in panes if p['right'] <= center]
    right_candidates = [p for p in panes if p['left'] >= center]
    if not left_candidates or not right_candidates: raise ValueError('divider lacks two adjacent content sides')
    left_edge=max(p['right'] for p in left_candidates);right_edge=min(p['left'] for p in right_candidates)
    if len({p['pid'] for p in panes})!=len(panes) or len({p['pane_id'] for p in panes})!=len(panes): raise ValueError('duplicate adjacent pane identity')
    if abs((left_edge+right_edge)/2-center)>1 or not 0 <= right_edge-left_edge <= handle[2]:
        raise ValueError('leaf allocation edges do not locate chosen divider')
    left=[p for p in left_candidates if p['right']==left_edge]
    right=[p for p in right_candidates if p['left']==right_edge]
    candidates=[]
    for a in left:
        for b in right:
            interval=[max(a['content_interval'][0],b['content_interval'][0],handle_interval[0]),min(a['content_interval'][1],b['content_interval'][1],handle_interval[1])]
            if interval[1] <= interval[0]: continue
            candidates.append(dict(left_pid=a['pid'],right_pid=b['pid'],left_pane_id=a['pane_id'],right_pane_id=b['pane_id'],
                interval=interval,axis_coverage=[a['left'],b['right']],eligible=interval[1]-interval[0]>=48 and a['left']<=roi_left and roi_right<=b['right']))
    candidates.sort(key=lambda p:(-(p['interval'][1]-p['interval'][0]),p['interval'][0],p['left_pane_id'],p['right_pane_id']))
    eligible=[p for p in candidates if p['eligible']]
    if not eligible: raise ValueError('no adequate header-free divider span')
    chosen=eligible[0]
    selected_left=next(p for p in left if p['pid']==chosen['left_pid'])
    selected_right=next(p for p in right if p['pid']==chosen['right_pid'])
    if selected_left['left']>round(center)-32 or selected_right['right']<round(center)+32:
        raise ValueError('adjacent panes too narrow for fixed divider ROI')
    start=(chosen['interval'][0]+chosen['interval'][1]-48)//2
    rect=[round(center)-32,start,64,48]
    if rect[0]<0 or rect[1]<0 or rect[0]+64>1600 or rect[1]+48>1000:
        raise ValueError('header-free divider ROI outside isolated screen')
    return dict(method='adjacent-terminal-content-longest-span-v1',tab_id=active_tab(snapshot)['tab_id'],
        paned_identity=divider['paned_identity'],adjacent_edges=[left_edge,right_edge],handle_interval=handle_interval,
        adjacent_content=left+right,candidate_intervals=candidates,chosen=chosen,rect=rect,expected=32)


def verify_divider_sampling_after(plan, after, origin):
    """Check fixed preselected cross-span against the same content identities."""
    observed=divider_sampling_plan(after,origin)
    if observed['tab_id']!=plan['tab_id'] or observed['paned_identity']!=plan['paned_identity']:
        raise ValueError('sampling changed tab/divider identity')
    chosen=plan['chosen']; start,end=plan['rect'][1],plan['rect'][1]+plan['rect'][3]
    matches=[p for p in observed['candidate_intervals'] if all(p[k]==chosen[k] for k in ('left_pid','right_pid','left_pane_id','right_pane_id'))]
    if len(matches)!=1 or not matches[0]['interval'][0] <= start < end <= matches[0]['interval'][1]:
        raise ValueError('preselected divider span lost same adjacent content coverage')
    if not matches[0]['axis_coverage'][0]<=plan['rect'][0]<plan['rect'][0]+plan['rect'][2]<=matches[0]['axis_coverage'][1]:
        raise ValueError('preselected ROI crosses an unrelated pane after movement')
    return dict(left_pid=chosen['left_pid'],right_pid=chosen['right_pid'],interval=matches[0]['interval'],covered_interval=[start,end],axis_coverage=matches[0]['axis_coverage'])


def expected_drag(snapshot, delta, origin):
    """GTK 4.14.5 gtkpaned.c: expanded handle origin, int update_drag, unclamped LTR.

    This predicts geometry BEFORE injecting; never derives expectation from the
    observed result. Narrow handle hit-area expansion is version-bound to 6px.
    """
    if snapshot.get('gtk_runtime_version') != [4, 14, 5] or any(type(v) is not int for v in snapshot['gtk_runtime_version']):
        raise ValueError('unsupported GtkPaned runtime version')
    if type(delta) is not int or abs(delta) != 12: raise ValueError('unsupported pointer delta')
    divider = snapshot['dividers'][0]
    if divider.get('wide_handle') is not False or divider.get('direction') != 'ltr':
        raise ValueError('unsupported wide/RTL GtkPaned mapping')
    if type(divider.get('paned_identity')) is not int or divider['paned_identity'] <= 0:
        raise ValueError('missing stable paned identity')
    if type(divider['horizontal']) is not bool: raise ValueError('invalid orientation')
    handles = divider['handle_bounds_local']
    if type(handles) is not list or len(handles) != 1: raise ValueError('ambiguous GtkPaned handle')
    handle = coordinates(handles[0], 4); bounds = coordinates(divider['paned_bounds'], 4)
    if min(handle[2:] + bounds[2:]) <= 0: raise ValueError('invalid paned/handle bounds')
    origin = coordinates(origin, 2)
    axis = 0 if divider['horizontal'] else 1
    pointer = [round(origin[i] + divider[['x','y'][i]]) for i in (0,1)]
    local_start = [pointer[i] - origin[i] - bounds[i] for i in (0,1)]
    hit_area = [handle[0]-6, handle[1]-6, handle[2]+12, handle[3]+12]
    if any(not hit_area[i] <= local_start[i] < hit_area[i]+hit_area[i+2] for i in (0,1)):
        raise ValueError('pointer misses measured narrow-handle hit area')
    for name in ('position','min_position','max_position'):
        if type(divider[name]) is not int: raise ValueError('invalid allocation limit')
    # drag_pos is int in GTK. update_drag also takes int coordinates.
    grab_offset = int(local_start[axis] - hit_area[axis])
    predicted_position = int(local_start[axis] + delta) - grab_offset
    if not divider['min_position'] < predicted_position < divider['max_position']:
        raise ValueError('clamped GtkPaned movement is unsupported')
    shift = predicted_position - divider['position']
    if shift != delta - 6 or shift * delta <= 0:
        raise ValueError('measured handle does not satisfy pinned narrow-handle mapping')
    return dict(model='gtk-4.14.5-narrow-ltr-unclamped', pointer_delta=delta,
                pointer_start_root=pointer, pointer_end_root=[pointer[i]+(delta if i==axis else 0) for i in (0,1)],
                pointer_start_local=local_start, handle_hit_area_local=hit_area,
                grab_offset=grab_offset, predicted_position=predicted_position, expected_allocation_shift=shift)


def allocation_shift(before, after, delta):
    finite(delta)
    if delta == 0: raise ValueError('zero drag delta')
    a = active_tab(before); b = active_tab(after)
    if a['tab_id'] != b['tab_id'] or [p['pid'] for p in a['panes']] != [p['pid'] for p in b['panes']]:
        raise ValueError('drag changed active tab or pane identity')
    x, y = before['dividers'][0], after['dividers'][0]
    if type(x['horizontal']) is not bool or type(y['horizontal']) is not bool or x['horizontal'] != y['horizontal']:
        raise ValueError('drag changed divider orientation')
    shift = finite(y['position']) - finite(x['position'])
    for pane in a['panes'] + b['panes']: coordinates(pane['bounds'], 4)
    if shift * delta <= 0 or abs(shift - delta) > 4:
        raise ValueError('real divider allocation did not follow drag')
    axis = (0, 2) if x['horizontal'] else (1, 3)
    other = (1, 3) if x['horizontal'] else (0, 2)
    if any(any(abs(p['bounds'][i] - q['bounds'][i]) > 1 for i in other) for p, q in zip(a['panes'], b['panes'])):
        raise ValueError('drag changed unrelated allocation axis')
    if all(all(p['bounds'][i] == q['bounds'][i] for i in axis) for p, q in zip(a['panes'], b['panes'])):
        raise ValueError('divider position changed without pane allocation')
    return shift


class PixelClient:
    def __init__(self, directory, env, check_cancelled):
        directory.mkdir(mode=0o700)
        self.check_cancelled = check_cancelled
        self.buffer = b''
        self.process = subprocess.Popen([sys.executable, '-I', str(Path(__file__).with_name('interaction-pixel-observer.py')),
                                         str(directory)], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        os.set_blocking(self.process.stdout.fileno(), False)

    def request(self, **command):
        encoded = (json.dumps(command) + '\n').encode()
        if len(encoded) > 4096: raise ValueError('oversized pixel observer command')
        self.process.stdin.write(encoded); self.process.stdin.flush()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            self.check_cancelled()
            if b'\n' in self.buffer:
                line, self.buffer = self.buffer.split(b'\n', 1)
                value = json.loads(line)
                if value.get('kind') != command['kind'] or value.get('name') != command['name']:
                    raise ValueError('pixel observer response identity mismatch')
                return value
            if self.process.poll() is not None: raise RuntimeError('owned pixel observer exited')
            if select.select([self.process.stdout], [], [], .025)[0]:
                self.buffer += os.read(self.process.stdout.fileno(), 32768)
                if len(self.buffer) > 65536: raise ValueError('pixel observer response exceeded bound')
        raise TimeoutError('owned pixel observer response deadline exceeded')

    def close_pipes(self):
        # OwnedProcesses, not this pipe helper, is the signal/reaping authority.
        for stream in (self.process.stdin, self.process.stdout):
            try: stream.close()
            except (BrokenPipeError, OSError): pass
