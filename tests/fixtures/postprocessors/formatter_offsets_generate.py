#!/usr/bin/env python3
"""Actual gallery-dl O argument parser, arithmetic, local TZ and jobs.

No offset grammar or arithmetic is copied. Every expected result is produced
by the installed formatter/MetadataPP. Local-time vectors run in isolated TZ
subprocesses and record host/Python provenance rather than assume zone rules.
"""
import hashlib
import base64
from tempfile import TemporaryDirectory
import inspect
import json
import os
import platform
import subprocess
import sys
import time
from pathlib import Path
from types import SimpleNamespace
from gallery_dl import dt,formatter,util,job
from gallery_dl.postprocessor import metadata
from formatter_dates_generate import encode,ModifyJob,run_job
import booru_defaults_generate as defaults


def evaluate(context,template):
    result={}
    for label,args in [('identity',(None,util.identity)),('default',())]:
        try:parsed=formatter.parse(template,*args)
        except Exception as exc:result[label]={'phase':'parse','error':type(exc).__name__,'message':str(exc)};continue
        try:result[label]={'output':encode(parsed.format_map(context))}
        except Exception as exc:result[label]={'phase':'render','error':type(exc).__name__,'message':str(exc)}
    return result


def modify_case(name,argument,value):
    options={'mode':'modify','fields':{'shifted':'{value:O'+argument+'/I/}','text':'{shifted!s}','epoch':'{shifted!T}',
               'repr':'{shifted!r}','hour':'{shifted.hour}','year':'{shifted.year}','formatted':'{shifted:%Y-%m-%d %H:%M:%S}'}}
    data={'value':value};result={'name':name,'argument':argument,'context':encode(data),'options':options}
    try:pp=metadata.MetadataPP(ModifyJob(),options)
    except Exception as exc:result['parse_error']={'class':type(exc).__name__,'message':str(exc)};return result
    pp.run(SimpleNamespace(kwdict=data));result['output']=encode(data);return result


def local_vectors():
    if hasattr(time,'tzset'):time.tzset()
    cases=[];modifies=[]
    inputs=[('epoch',dt.datetime(1970,1,1)),('winter',dt.datetime(2024,1,2,3,4,5)),
            ('summer',dt.datetime(2024,7,2,3,4,5)),('dst_spring_before',dt.datetime(2024,3,31,0,59,59)),
            ('dst_spring_after',dt.datetime(2024,3,31,1,0,0)),('dst_fall_before',dt.datetime(2024,10,27,0,59,59)),
            ('dst_fall_after',dt.datetime(2024,10,27,1,0,0)),('historical',dt.datetime(1890,1,2,3,4,5)),
            ('null',dt.NONE),('upper',dt.datetime(9999,12,31,23,59,59)),
            ('aware_utc',dt.datetime(2024,1,2,3,4,5,tzinfo=dt.timezone.utc))]
    for name,value in inputs:
        for argument in ('','local'):
            for spec in ('I/','%Y-%m-%d %H:%M:%S','CT/'):
                template='{value:O'+argument+'/'+spec+'}'
                entry={'name':name+'_'+argument+'_'+spec,'argument':argument,'context':encode({'value':value}),
                       'template':template,**evaluate({'value':value},template)}
                if value.tzinfo is not None:entry['native_representation_boundary']='preexisting aware datetime, including UTC, is not represented by epoch-only native Date snapshots'
                cases.append(entry)
            modified=modify_case(name+'_'+argument,argument,value)
            if value.tzinfo is not None:modified['native_representation_boundary']='preexisting aware datetime, including UTC, is not represented by epoch-only native Date snapshots'
            modifies.append(modified)
    # Source local lookup converts timedelta to float seconds. At late-year
    # DST transitions that float can cross a boundary while the input datetime
    # still retains its preceding second and microseconds.
    for name,value in [('late_dst_before',dt.datetime(9998,3,29,0,59,59,999900)),
                       ('late_dst_rounded_boundary',dt.datetime(9998,3,29,0,59,59,999999)),
                       ('late_dst_exact',dt.datetime(9998,3,29,1,0,0)),
                       ('late_dst_after',dt.datetime(9998,3,29,1,0,0,1))]:
        for argument in ('','local'):
            for spec in ('I/','%Y-%m-%d %H:%M:%S.%f','CT/'):
                template='{value:O'+argument+'/'+spec+'}'
                cases.append({'name':name+'_'+argument+'_'+spec,'argument':argument,'context':encode({'value':value}),
                              'template':template,**evaluate({'value':value},template),'requires_typed_context':True})
    unterminated=[]
    value=dt.datetime(2024,1,2,3,4,5)
    for argument in ('','local'):
        template='{value:O'+argument+'}'
        unterminated.append({'name':'local_unterminated_'+argument,'argument':argument,'context':encode({'value':value}),
                             'template':template,**evaluate({'value':value},template)})
    return {'tz':os.environ.get('TZ'),'platform':platform.platform(),'python':sys.version.split()[0],
            'time_tzname':list(time.tzname),'cases':cases,'modify_cases':modifies,'unterminated_cases':unterminated}


def build():
    arguments=['0','+0','-0','1','+1','-1','+00:30','-00:30','00:30','+00:-30','-00:-30',
               '+01:30','-01:30','1:-30','-1:-30','25','-25','25:90','0:90','1:','1:0','1:2:3',
               ' 1 ',' 1 : 30 ','+１:３０','١:٣٠','1_0:3_0','1.5','PRIVATE_BAD_ARGUMENT','Local',
               str(24*999999999),str(24*1000000000),'9'*1000]
    huge=10**200
    arguments += [str(huge)+':'+str(-60*huge),str(-huge)+':'+str(-60*huge),
                  str(huge)+':'+str(-60*huge+1),str(huge)+':'+str(-60*huge-1),
                  '23999999999:59','24000000000:00','-23999999976:00','-23999999976:01',
                  '0:'+str(999999999*1440),'0:'+str(999999999*1440+1),
                  '0'*4300,'0'*4301,'0'*4299+'1','0'*4300+'1',
                  '0_'*4299+'1','0_'*4300+'1']
    contexts=[('normal',dt.datetime(2024,1,2,3,4,5)),('null',dt.NONE),
              ('lower',dt.datetime(1,1,1)),('upper',dt.datetime(9999,12,31,23,59,59))]
    cases=[];modifies=[]
    for ai,argument in enumerate(arguments):
        for name,value in contexts:
            for spec in ('I/','%Y-%m-%d %H:%M:%S','CT/'):
                template='{value:O'+argument+'/'+spec+'}'
                cases.append({'name':str(ai)+'_'+name+'_'+spec,'argument':argument,'context':encode({'value':value}),
                              'template':template,**evaluate({'value':value},template)})
        for name,value in contexts[:2]:modifies.append(modify_case(str(ai)+'_'+name,argument,value))
    aware=dt.datetime(2024,1,2,3,4,5,tzinfo=dt.timezone.utc)
    for argument in ('0','+00:30'):
        for spec in ('I/','%Y-%m-%d %H:%M:%S','CT/'):
            template='{value:O'+argument+'/'+spec+'}'
            cases.append({'name':'aware_utc_'+argument+'_'+spec,'argument':argument,'context':encode({'value':aware}),
                          'template':template,**evaluate({'value':aware},template),
                          'native_representation_boundary':'preexisting aware datetime, including UTC, is not represented by epoch-only native Date snapshots'})
        modified=modify_case('aware_utc_'+argument,argument,aware)
        modified['native_representation_boundary']='preexisting aware datetime, including UTC, is not represented by epoch-only native Date snapshots'
        modifies.append(modified)
    root=next(item for item in defaults.ROOTS if item[0]=='danbooru_post')
    jobs=[]
    for argument in ('+00:30','-00:30','25:90','+１:３０'):
        options={'directory':['offset','{date:O'+argument+'/%Y-%m-%d}'],'filename':'{shifted:%Y%m%d%H%M%S}_{id}.{extension}'}
        hooks=[{'name':'metadata','mode':'modify','event':'post','fields':{'shifted':'{date:O'+argument+'/I/}'}},
               {'name':'metadata','mode':'custom','event':'file','filename':'offset.txt','format':'{shifted:%Y-%m-%d %H:%M:%S}|{shifted!T}'},
               {'name':'metadata','mode':'jsonl','event':'file','filename':'offsets.jsonl','base-directory':True,'directory':[],
                'include':['id','shifted'],'archive':[':basedirectory','metadata.sqlite'],'archive-format':'{shifted:%Y%m%d%H%M%S}_{id}'}]
        case=run_job(root,'offset_'+argument,overrides=options,hook_options=hooks)
        import sqlite3
        sql=[];published=[]
        for item in case['published']:
            if item['path'].endswith('.sqlite'):
                connection=sqlite3.connect(':memory:');connection.deserialize(bytes.fromhex(item['data_hex']))
                sql.extend(row[0] for row in connection.execute('SELECT entry FROM archive ORDER BY entry'));connection.close()
            else:published.append(item)
        case['published']=published;case['archive_sql']=sql;case['argument']=argument;jobs.append(case)
    local=[]
    for zone in ('UTC','Europe/Oslo','Asia/Kathmandu'):
        result=subprocess.run([sys.executable,'-B',str(Path(__file__).resolve()),'--local-worker'],
                              env={**os.environ,'TZ':zone},capture_output=True,text=True,check=True)
        group=json.loads(result.stdout)
        candidates=[Path('/usr/share/zoneinfo')/zone,Path('/var/db/timezone/zoneinfo')/zone]
        zonefile=next(path for path in candidates if path.is_file())
        content=zonefile.read_bytes()
        with TemporaryDirectory(prefix='source-tzif-',dir='/private/tmp') as directory:
            copied=Path(directory)/'zone.tzif';copied.write_bytes(content)
            verified=subprocess.run([sys.executable,'-B',str(Path(__file__).resolve()),'--local-worker'],
                env={**os.environ,'TZ':':'+str(copied)},capture_output=True,text=True,check=True)
            file_group=json.loads(verified.stdout)
            assert {key:value for key,value in group.items() if key!='tz'} == {key:value for key,value in file_group.items() if key!='tz'}, zone
        group['tzif']={'source_path':str(zonefile),'resolved_path':str(zonefile.resolve()),
                       'sha256':hashlib.sha256(content).hexdigest(),'base64':base64.b64encode(content).decode('ascii'),
                       'byte_length':len(content),'verified_tz_template':':$TZIF','verified_all_observations_equal':True}
        local.append(group)
    return cases,modifies,jobs,local


def unterminated_cases():
    value=dt.datetime(2024,1,2,3,4,5)
    cases=[]
    for argument in ('1','+00:30','1:','1:2:3'):
        template='{value:O'+argument+'}'
        cases.append({'name':'unterminated_'+argument,'argument':argument,'context':encode({'value':value}),
                      'template':template,**evaluate({'value':value},template)})
    return cases


if __name__=='__main__':
    if '--local-worker' in sys.argv:
        print(json.dumps(local_vectors(),ensure_ascii=True));sys.exit(0)
    cases,modifications,jobs,local=build()
    files={'formatter':Path(inspect.getfile(formatter)),'dt':Path(inspect.getfile(dt)),
           'metadata':Path(inspect.getfile(metadata)),'job':Path(inspect.getfile(job)),
           'typed_runner':Path(sys.modules[run_job.__module__].__file__),'job_runner':Path(defaults.__file__)}
    result={'source':'gallery-dl','version':'1.32.17','platform':platform.platform(),'python':sys.version,
            'source_sha256':{name:hashlib.sha256(path.read_bytes()).hexdigest() for name,path in files.items()},
            'cases':cases,'modify_cases':modifications,'job_cases':jobs,'local_cases':local,'unterminated_cases':unterminated_cases()}
    Path(__file__).with_name('formatter_offsets.json').write_text(json.dumps(result,ensure_ascii=True,indent=2)+'\n')
