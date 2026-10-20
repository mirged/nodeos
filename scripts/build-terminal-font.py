#!/usr/bin/env python3
"""Bake terminal glyph coverage using system FreeType; no runtime dependency.
Usage: python3 scripts/build-terminal-font.py /path/to/DejaVuSansMono.ttf
"""
import ctypes as c
from pathlib import Path
import sys
class Generic(c.Structure): _fields_=[('data',c.c_void_p),('finalizer',c.c_void_p)]
class Box(c.Structure): _fields_=[(n,c.c_long) for n in ('xMin','yMin','xMax','yMax')]
class Vector(c.Structure): _fields_=[('x',c.c_long),('y',c.c_long)]
class Metrics(c.Structure): _fields_=[(n,c.c_long) for n in ('width','height','horiBearingX','horiBearingY','horiAdvance','vertBearingX','vertBearingY','vertAdvance')]
class Bitmap(c.Structure): _fields_=[('rows',c.c_uint),('width',c.c_uint),('pitch',c.c_int),('buffer',c.POINTER(c.c_ubyte)),('num_grays',c.c_ushort),('pixel_mode',c.c_ubyte),('palette_mode',c.c_ubyte),('palette',c.c_void_p)]
class Slot(c.Structure): _fields_=[('library',c.c_void_p),('face',c.c_void_p),('next',c.c_void_p),('glyph_index',c.c_uint),('generic',Generic),('metrics',Metrics),('linearHoriAdvance',c.c_long),('linearVertAdvance',c.c_long),('advance',Vector),('format',c.c_uint),('bitmap',Bitmap),('bitmap_left',c.c_int),('bitmap_top',c.c_int)]
class Face(c.Structure): _fields_=[('num_faces',c.c_long),('face_index',c.c_long),('face_flags',c.c_long),('style_flags',c.c_long),('num_glyphs',c.c_long),('family_name',c.c_char_p),('style_name',c.c_char_p),('num_fixed_sizes',c.c_int),('available_sizes',c.c_void_p),('num_charmaps',c.c_int),('charmaps',c.c_void_p),('generic',Generic),('bbox',Box),('units_per_EM',c.c_ushort),('ascender',c.c_short),('descender',c.c_short),('height',c.c_short),('max_advance_width',c.c_short),('max_advance_height',c.c_short),('underline_position',c.c_short),('underline_thickness',c.c_short),('glyph',c.POINTER(Slot))]
ft=c.CDLL('libfreetype.so.6')
ft.FT_Init_FreeType.argtypes=[c.POINTER(c.c_void_p)]
ft.FT_New_Face.argtypes=[c.c_void_p,c.c_char_p,c.c_long,c.POINTER(c.POINTER(Face))]
ft.FT_Set_Pixel_Sizes.argtypes=[c.POINTER(Face),c.c_uint,c.c_uint]
ft.FT_Load_Char.argtypes=[c.POINTER(Face),c.c_ulong,c.c_int]
ft.FT_Done_Face.argtypes=[c.POINTER(Face)]
ft.FT_Done_FreeType.argtypes=[c.c_void_p]
lib=c.c_void_p(); assert ft.FT_Init_FreeType(c.byref(lib))==0
face=c.POINTER(Face)(); assert ft.FT_New_Face(lib,sys.argv[1].encode(),0,c.byref(face))==0
out=Path(__file__).resolve().parents[1]/'user/libdraw/src/terminal-font'
for name,size,width,height,baseline in [('small',13,8,18,14),('normal',16,10,22,17),('large',20,13,26,21)]:
 assert ft.FT_Set_Pixel_Sizes(face,0,size)==0
 atlas=bytearray()
 for char in range(32,127):
  assert ft.FT_Load_Char(face,char,4)==0
  slot=face.contents.glyph.contents; bitmap=slot.bitmap
  assert bitmap.pixel_mode==2
  cell=bytearray(width*height)
  for y in range(bitmap.rows):
   for x in range(bitmap.width):
    dx=x+slot.bitmap_left;dy=y+baseline-slot.bitmap_top
    value=bitmap.buffer[y*bitmap.pitch+x]
    if 0<=dx<width and 0<=dy<height:cell[dy*width+dx]=value
    else:assert value==0,(name,chr(char),dx,dy)
  atlas.extend(cell)
 (out/(name+'.alpha')).write_bytes(atlas)
 print(name,len(atlas),'bytes')
ft.FT_Done_Face(face);ft.FT_Done_FreeType(lib)
