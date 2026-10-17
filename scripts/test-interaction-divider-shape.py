#!/usr/bin/env python3
"""Synthetic pre-input strip correspondence, independent of retained artifacts."""
import importlib.util
from pathlib import Path
import unittest
spec=importlib.util.spec_from_file_location('observer',Path(__file__).with_name('interaction-pixel-observer.py'))
o=importlib.util.module_from_spec(spec);spec.loader.exec_module(o)
class Shape(unittest.TestCase):
 def image(self, strips=(), noise=(), partial=(), width=1, color=220):
  pixels=[[20]*64 for _ in range(48)]
  for x in strips:
   for row in pixels:
    for p in range(x,x+width):row[p]=color
  for x,y0,y1,value in noise:
   for y in range(y0,y1):pixels[y][x]=value
  for x,y0,y1 in partial:
   for y in range(y0,y1):pixels[y][x]=color
  return bytes(v for row in pixels for p in row for v in (p,p,p))
 def call(self,a,b,delta=12):return o.divider_motion(a,b,64,48,horizontal=True,expected=32,delta=delta)
 def pair(self,**kwargs):return self.image((32,)),self.image((44,),noise=((32,0,12,80),),**kwargs)
 def test_unrelated_partial_body_edge(self):
  a,b=self.pair();r=self.call(a,b);self.assertEqual(r['disappearance_proof']['method'],'unique-preinput-paired-strip-v1')
 def test_negative_direction(self):
  a=self.image((32,));b=self.image((14,),noise=((32,0,12,80),));self.assertEqual(self.call(a,b,-18)['disappearance_proof']['after_start'],14)
 def test_two_pixel_strip(self):
  a=self.image((32,),width=2);b=self.image((44,),width=2,noise=((32,0,12,80),));self.assertEqual(self.call(a,b)['disappearance_proof']['width'],2)
 def test_stationary(self):
  with self.assertRaises(ValueError):self.call(self.image((32,44)),self.image((32,44)))
 def test_partial_old_residual(self):
  a,b=self.pair(partial=((32,20,21),))
  with self.assertRaises(ValueError):self.call(a,b)
 def test_preexisting_partial_target(self):
  a=self.image((32,),partial=((44,20,21),));b=self.image((44,),noise=((32,0,12,80),))
  with self.assertRaises(ValueError):self.call(a,b)
 def test_duplicate(self):
  with self.assertRaises(ValueError):self.call(self.image((32,44)),self.image((44,)))
 def test_occluded_target(self):
  a,b=self.pair();b=bytearray(b);b[(10*64+44)*3:(10*64+44)*3+3]=b'\x14'*3
  with self.assertRaises(ValueError):self.call(a,b)
 def test_wide_body_lookalike(self):
  with self.assertRaises(ValueError):self.call(self.image((32,),width=5),self.image((44,),width=5,noise=((32,0,12,80),)))
 def test_color_change(self):
  with self.assertRaises(ValueError):self.call(self.image((32,)),self.image((44,),color=210,noise=((32,0,12,80),)))
 def test_wrong_shift(self):
  with self.assertRaises(ValueError):self.call(self.image((32,)),self.image((48,),noise=((32,0,12,80),)))
 def test_preinput_cross_axis_not_stable(self):
  a=bytearray(self.image((32,)));a[(10*64+32)*3:(10*64+32)*3+3]=bytes([219]*3)
  with self.assertRaises(ValueError):self.call(a,self.image((44,),noise=((32,0,12,80),)))
 def test_transpose(self):
  a,b=self.pair()
  def tr(rgb):return bytes(rgb[(y*64+x)*3+c] for x in range(64) for y in range(48) for c in range(3))
  r=o.divider_motion(tr(a),tr(b),48,64,horizontal=False,expected=32,delta=12);self.assertEqual(r['disappearance_proof']['width'],1)
if __name__=='__main__':unittest.main()
