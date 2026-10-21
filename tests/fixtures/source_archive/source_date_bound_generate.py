#!/usr/bin/env python3
"""gallery-dl 1.32.17 dt.convert and real DownloadJob bound oracle.

Expected values come only from installed dt.convert; no ISO/numeric parsing
algorithm is reproduced. Selected jobs use frozen booru_date_selection.run,
which observes genuine predicates and actual DownloadJob publication. JSON
ASCII escaping deliberately retains unpaired-surrogate input strings.
"""
import hashlib
import inspect
import json
from pathlib import Path
from gallery_dl import dt,job
import booru_date_selection_generate as jobs


def inputs():
    cases=[];seen=set()
    def add(name,value):
        key=json.dumps(value,ensure_ascii=True,separators=(',',':'))
        if key not in seen:seen.add(key);cases.append((name,value))
    for name,value in [('null',None),('false',False),('true',True),('zero',0),('one',1),('negative',-1),
                       ('positive_float',1.5),('negative_float',-1.5),('subsecond',0.1),('negative_subsecond',-0.1),
                       ('large_float',1e100),('small_float',1e-100),('big_integer',10**80),('negative_big_integer',-(10**80)),
                       ('empty_array',[]),('array',[1]),('empty_object',{}),('object',{'date':'2024-01-02'})]:add(name,value)
    for value in ('',' ','0','-0','+0','1.5','-1.5','1e3','NaN','Infinity','+123','-123','  +123  ',
                  '１２３','١٢٣','𝟙𝟚𝟛','1_000','+1_000','1__0','_10','10_','1 0','0x10','００００',
                  '20240102',' 20240102 ','20240000','20241301','20240230','00010101','99991231',
                  '-62135596800','-62135596801','253402300799','253402300800',
                  '9'*1000,'9'*4301,' '*140+'123'+' '*140):add('numeric_'+str(len(cases)),value)
    for space in ('\t','\n','\r','\v','\f','\x1c','\x1d','\x1e','\x1f','\u0085','\u00a0','\u1680','\u2000','\u2028','\u202f','\u3000','\u200b','\ufeff'):
        add('numeric_space_'+hex(ord(space)),space+'123'+space)
    dates=['2024-01-02','20240102','2024-W01','2024-W01-2','2024W01','2024W012','2024-W53-1','2020-W53-7',
           '2021-W53-1','2024-W00-1','2024-W01-0','2024-W01-8','2024-w01-1','2024-002','2024-01','2024',
           '2024-1-02','2024-01-2','0000-01-01','0001-01-01','9999-12-31','10000-01-01',
           '2024-02-29','2023-02-29','1900-02-29','2000-02-29','２０２４-０１-０２']
    for value in dates:add('calendar_'+str(len(cases)),value)
    for value in (' 2024-01-02','2024-01-02 ','\t2024-01-02','2024-01-02\n','2024-01-02Z','20240102Z'):
        add('calendar_space_'+str(len(cases)),value)
    for separator in ('T','t',' ','\t','\n','\r','\x00','猫','😀','\ud800','\udfff','TT','  '):
        add('separator_'+str(len(cases)),'2024-01-02'+separator+'03:04:05')
    clocks=['03','0304','030405','03:04','03:04:05','3','3:04','03:4','03:04:5','03:0405','0304:05',
            '24','24:00','23:59:60','03.5','03,5','03:04.5','03:04,5','0304.5','030405.5','03:04:05.5',
            '03:04:05,123456','03:04:05.','03:04:05,','03:04:05.1234567','03:04:05.1234567890123456789']
    for date in ('2024-01-02','20240102','2024-W01-2','2024W012'):
        for clock in clocks:add('time_'+str(len(cases)),date+'T'+clock)
    for value in ('2024-01-02T03:04:05.123456Z','2024-01-02T03:04:05Z','2024-01-02T03:04:05z',
                  '2024-01-02T03:04:05ZZ','2024-01-02T03:04:05+00:00Z',
                  '2024-01-02T03:04:05.'+'1'*140+'Z','2024-01-02T03:04:05.'+'9'*5000+'Z'):
        add('fraction_z_'+str(len(cases)),value)
    zones=['+01','-01','+1','+001','+0102','-0102','+01:02','+010203','-010203','+01:02:03','-01:02:03',
           '+01:02:03.123456','-01:02:03.123456','+010203,123456','+01.5','+01:02.5',
           '+00:00:00.5','-00:00:00.5','+00:00:00.000000','+00:00:00.000001',
           '+00:60','+00:99','+01:00:60','+23:59:59','-23:59:59','+23:59:60','+24:00','-24:00',
           '+01:02:03.','+01:02:03.1234567890123456789','+01:02:03.'+'1'*140]
    for local in ('2024-01-02T03:04:05','2024-01-02T03:04:05.987654'):
        for zone in zones:add('offset_'+str(len(cases)),local+zone)
    for value in ('0001-01-01T00:00:00+00:00','0001-01-01T00:00:00+00:00:01',
                  '0001-01-01T00:00:00-23:59:59','9999-12-31T23:59:59-00:00:01',
                  '9999-12-31T23:59:59+23:59:59','9999-12-31T23:59:59.999999Z'):
        add('utc_year_boundary_'+str(len(cases)),value)
    # Genuine CPython C fromisoformat accepts implicit fractions only after
    # uncolonized HHMMSS, including uncolonized timezone clocks.
    for width in (1,2,6,7,130,4301):
        tail='1'*width
        add('implicit_clock_'+str(width),'20240101T123456'+tail)
        add('implicit_zone_'+str(width),'20240101T123456+010203'+tail)
    for tail in ('１２','١٢','1２','ab','12x','12猫'):
        add('implicit_clock_invalid_'+str(len(cases)),'20240101T123456'+tail)
        add('implicit_zone_invalid_'+str(len(cases)),'20240101T123456+010203'+tail)
    for value in ('2024-01-01T12:34:5612','2024-01-01T12:34:561',
                  '20240101T123456+01:02:0312','20240101T123456+01:02:031',
                  '20240101T123456123Z','20240102T03040578+00000045','1'*4301):
        add('implicit_mixed_'+str(len(cases)),value)
    return cases


def build():
    conversions=[]
    for name,value in inputs():
        converted=dt.convert(value)
        item={'name':name,'python_type':type(value).__name__,
                            'converted_type':type(converted).__name__,'normalized':str(converted),
                            'epoch':int(dt.to_ts(converted)) if converted else None,'truthy':bool(converted)}
        if isinstance(value,str) and any(0xD800<=ord(c)<=0xDFFF for c in value):
            item.update(representable_json=False,value_codepoints=[ord(c) for c in value],value_python_repr=repr(value))
        else:item.update(representable_json=True,value=value)
        conversions.append(item)
    selected=[
        ('compact','20240102'),('compact_outer_space',' 20240102 '),('iso_outer_space',' 2024-01-02 '),
        ('week','2024-W01-2'),('basic','20240102T030405'),('hour','2024-01-02T03'),
        ('unicode_separator','2024-01-02猫03:04:05'),
        ('nul_separator','2024-01-02\x0003:04:05'),('fractional_offset','2024-01-02T03:04:05+01:02:03.123456'),
        ('zero_fractional_offset','2024-01-02T03:04:05+00:00:00.5'),
        ('long_fraction','2024-01-02T03:04:05.'+'1'*140+'Z'),('long_numeric_space',' '*140+'123'+' '*140),
        ('unicode_numeric','１２３'),('underscored_numeric','1_000'),('exponent_string','1e3'),
        ('json_float',1.5),('json_boolean',True),('string_zero','0'),('numeric_zero',0),
        ('utc_underflow','0001-01-01T00:00:00+00:00:01')]
    job_cases=[]
    for family in ('danbooru','e621','e926'):
        for name,value in selected:
            job_cases.append(jobs.run(family,'bound_'+name,{'date-before':value},mode=False))
    for family in ('danbooru','e621','e926'):
        job_cases.append(jobs.run(family,'implicit_fraction_before_equal',{'date-before':'20240102T03040512'},mode=False))
        job_cases.append(jobs.run(family,'implicit_fraction_offset_after_equal',{'date-after':'20240102T03040578+00000045'},mode=False))
    return conversions,job_cases


if __name__=='__main__':
    conversions,job_cases=build()
    sources={'dt':Path(inspect.getfile(dt)),'job':Path(inspect.getfile(job)),'date_job_runner':Path(jobs.__file__)}
    result={'source':'gallery-dl','version':'1.32.17','python_version':__import__('sys').version.split()[0],
            'frozen_today':'2026-10-21','source_sha256':{name:hashlib.sha256(path.read_bytes()).hexdigest() for name,path in sources.items()},
            'conversion_cases':conversions,'job_cases':job_cases}
    Path(__file__).with_name('source_date_bound.json').write_text(json.dumps(result,ensure_ascii=True,indent=2)+'\n')
