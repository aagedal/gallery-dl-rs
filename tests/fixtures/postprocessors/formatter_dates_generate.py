#!/usr/bin/env python3
"""Actual source D/d formatter identity, MetadataPP and path publication.

No conversion algorithm is copied. Bound input matrix comes from the frozen
source-date generator. Real formatter/MetadataPP/default booru DownloadJob run.
Typed descriptors preserve datetime vs NullDatetime vs ordinary primitives.
"""
import copy
import hashlib
import inspect
import json
import logging
import sys
from pathlib import Path
from types import SimpleNamespace
from gallery_dl import dt,formatter,util,job
from gallery_dl.postprocessor import metadata
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'source_archive'))
import source_date_bound_generate as bounds
import booru_defaults_generate as defaults


def encode(value):
    if isinstance(value,dt.datetime):
        try:epoch=dt.to_ts(value);epoch_error=None
        except Exception as exc:epoch=None;epoch_error=type(exc).__name__
        result={'kind':type(value).__name__,'iso':value.isoformat(),'text':str(value),'truthy':bool(value),
                'epoch':epoch,'year_format':format(value,'%Y'),'public_json':json.dumps(value,default=util.json_default),
                'microsecond':value.microsecond,'offset_seconds':value.utcoffset().total_seconds() if value.utcoffset() else 0}
        if epoch_error:result['epoch_error']=epoch_error
        return result
    if value is util.NONE:return {'kind':'formatter_default','text':str(value),'truthy':bool(value)}
    if isinstance(value,(tuple,list)):return {'kind':'list','items':[encode(item) for item in value]}
    if isinstance(value,dict):return {'kind':'map','entries':{key:encode(item) for key,item in value.items()}}
    if isinstance(value,str) and any(0xD800<=ord(c)<=0xDFFF for c in value):
        return {'kind':'python_string','codepoints':[ord(c) for c in value],'representable_json':False}
    return {'kind':'value','value':value}


class ModifyJob:
    def get_logger(self,*args):return logging.getLogger('formatter-dates-source')
    def register_hooks(self,*args):pass


def evaluate(context,template):
    result={}
    for name,args in [('identity',(None,util.identity)),('default',())]:
        try:result[name]={'output':encode(formatter.parse(template,*args).format_map(context))}
        except Exception as exc:result[name]={'error':type(exc).__name__,'message':str(exc)}
    return result


def run_job(*args,**kwargs):
    """Observe the real runner callback dictionaries before type erasure."""
    typed_trace=[]
    original=job.DownloadJob.register_hooks
    def register(self,hooks,options=None):
        observed={}
        for event,callback in hooks.items():
            if getattr(callback,'__module__',None)==defaults.__name__:
                def checkpoint(pathfmt,callback=callback,event=event):
                    callback(pathfmt)
                    data={key:value for key,value in pathfmt.kwdict.items() if key not in ('_path','_extr')}
                    typed_trace.append({'event':event,'metadata':encode(data)})
                observed[event]=checkpoint
            else:observed[event]=callback
        return original(self,observed,options)
    job.DownloadJob.register_hooks=register
    try:result=defaults.run_case(*args,**kwargs)
    finally:job.DownloadJob.register_hooks=original
    result['typed_trace']=typed_trace
    return result


def build():
    conversions=[];modifications=[];jobs=[]
    for name,value in bounds.inputs():
        context={'value':value}
        for conv in ('D','d'):
            template='{value!'+conv+'}'
            conversions.append({'name':'bound_'+name+'_'+conv,'context':encode(context),'template':template,**evaluate(context,template)})
    special=[('epoch',dt.datetime(1970,1,1)),('utc_micro',dt.datetime(2024,1,2,3,4,5,123456)),
             ('nonutc_micro',dt.datetime(2024,1,2,3,4,5,123456,tzinfo=dt.timezone(dt.timedelta(hours=5,minutes=30,seconds=1,microseconds=500000)))),
             ('null_datetime',dt.NONE),('none',None),('literal_invalid','[Invalid DateTime]'),('zero',0),('false',False),('true',True),
             ('negative_fraction',-0.9),('fraction',1.9),('unicode_integer','١_７００_０００_０００'),('empty',''),('invalid','PRIVATE_INVALID_DATE')]
    for name,value in special:
        context={'value':value,'values':[value],'mapping':{'date':value},'converted':dt.convert(value)}
        templates=['{value}','x{value}y','{missing!D}','{missing!d}','{missing|value!D}',
                   '{values[0]!D}','{mapping[date]!d}','{converted.year}','{converted.month}','{converted.day}',
                   '{converted!s}','{converted!j}','{converted!T}','{value:CDs/}','{value:Cdj/}',
                   '{value!D:Cs/}','{value!d:Cs/}','prefix{value!D}suffix','{value!D}|{value!d}']
        for conv in ('D','d'):
            for spec in ('','%Y','%Y-%m-%d %H:%M:%S','%f','%z','>20','03','I/','?YES/NO/','Cs/','Cj/','O+02:00/%H:%M'):
                templates.append('{value!'+conv+(':'+spec if spec else '')+'}')
        for template in dict.fromkeys(templates):
            conversions.append({'name':'special_'+name,'context':encode(context),'template':template,**evaluate(context,template),
                                'preexisting_subsecond_or_aware':isinstance(value,dt.datetime) and (value.microsecond!=0 or value.tzinfo is not None)})
        for conv in ('D','d'):
            options={'mode':'modify','fields':{'copied':'{value!'+conv+'}','plain':'{copied}','year':'{copied:%Y}',
                       'attribute_year':'{copied.year}','json':'{copied!j}','epoch':'{copied!T}','conditional':'{copied:?YES/NO/}',
                       'raw':'{value}','multi':'x{copied}y'}}
            data=copy.deepcopy(context)
            pp=metadata.MetadataPP(ModifyJob(),options)
            pp.run(SimpleNamespace(kwdict=data))
            modifications.append({'name':name+'_'+conv,'context':encode(context),'options':options,'output':encode(data),
                                  'preexisting_subsecond_or_aware':isinstance(value,dt.datetime) and (value.microsecond!=0 or value.tzinfo is not None)})
    for label,assignment in [('literal_iso','2030-02-03T04:05:06'),('numeric','{number}'),('null','{nothing}')]:
        context={'date':dt.datetime(2024,1,2,3,4,5),'number':123,'nothing':None}
        fields={'date':assignment,'date_updated':assignment,'date_url':assignment,
                'copied':'{date}','copied_updated':'{date_updated}','copied_url':'{date_url}',
                'year':'{copied:%Y}','explicit':'{date!D}','explicit_year':'{explicit:%Y}'}
        options={'mode':'modify','fields':fields};data=copy.deepcopy(context)
        metadata.MetadataPP(ModifyJob(),options).run(SimpleNamespace(kwdict=data))
        modifications.append({'name':'assigned_date_keys_'+label,'context':encode(context),'options':options,'output':encode(data),
                              'preexisting_subsecond_or_aware':False})
    context={'date':dt.datetime(2024,1,2,3,4,5)}
    options={'mode':'modify','fields':{'copied':'{date}','year':'{copied:%Y}','attribute_year':'{copied.year}'}}
    data=copy.deepcopy(context);metadata.MetadataPP(ModifyJob(),options).run(SimpleNamespace(kwdict=data))
    modifications.append({'name':'copy_original_date','context':encode(context),'options':options,'output':encode(data),
                          'preexisting_subsecond_or_aware':False})
    root=next(item for item in defaults.ROOTS if item[0]=='danbooru_post')
    for name,value,conv in [('compact','20240102T030405','D'),('implicit','20240102T03040578+00000045','D'),
                            ('zero_D',0,'D'),('zero_d',0,'d'),('invalid','PRIVATE_INVALID_DATE','D'),('negative',-1.5,'d')]:
        options={'directory':['typed','{raw!'+conv+':%Y}'],'filename':'{copied:%Y}_{id}.{extension}'}
        hooks=[{'name':'metadata','mode':'modify','event':'post','fields':{'copied':'{raw!'+conv+'}','year':'{copied:%Y}'}},
               {'name':'metadata','mode':'custom','event':'file','filename':'typed.txt','format':'{copied:%Y}|{year}|{copied!T}'},
               {'name':'metadata','mode':'jsonl','event':'file','filename':'dates.jsonl','base-directory':True,'directory':[],
                'include':['id','copied','year'],'archive':[':basedirectory','metadata.sqlite'],'archive-format':'{copied:%Y}_{id}'}]
        case=run_job(root,'typed_'+name,overrides=options,hook_options=hooks,input_overrides={'raw':value})
        # Decode real database bytes into stable logical keys; preserve every
        # other published artifact byte-for-byte from actual source job.
        import sqlite3
        sql=[];published=[]
        for artifact in case['published']:
            if artifact['path'].endswith('.sqlite'):
                connection=sqlite3.connect(':memory:');connection.deserialize(bytes.fromhex(artifact['data_hex']))
                sql.extend(row[0] for row in connection.execute('SELECT entry FROM archive ORDER BY entry'));connection.close()
            else:published.append(artifact)
        case['published']=published;case['archive_sql']=sql;jobs.append(case)
    extra_jobs=[('copy_original_date',{'directory':['original','{date:%Y}'],'filename':'{copied:%Y}_{id}.{extension}'},
        [{'name':'metadata','mode':'modify','event':'post','fields':{'copied':'{date}','year':'{copied:%Y}'}},
         {'name':'metadata','mode':'custom','event':'file','filename':'copy.txt','format':'{copied:%Y}|{year}'}]),
        ('assigned_literal_date',{'directory':['original','{date:%Y}'],'filename':'{id}.{extension}'},
        [{'name':'metadata','mode':'modify','event':'post','fields':{'date':'2030-02-03T04:05:06','date_updated':'2030-02-03T04:05:06',
          'date_url':'2030-02-03T04:05:06','copied':'{date}','explicit':'{date!D}'}},
         {'name':'metadata','mode':'custom','event':'file','filename':'literal.txt','format':'{date!u}|{copied!u}|{explicit:%Y}',
          'filter':"date == '2030-02-03T04:05:06'"}]),
        ('assigned_numeric_date',{'directory':['original','{date:%Y}'],'filename':'{id}.{extension}'},
        [{'name':'metadata','mode':'modify','event':'post','fields':{'date':'{id}','date_updated':'{id}','date_url':'{id}',
          'copied':'{date}','explicit':'{date!D}'}},
         {'name':'metadata','mode':'custom','event':'file','filename':'numeric.txt','format':'{date}|{copied}|{explicit:%Y}',
          'filter':'date == 42'}])]
    for name,options,hooks in extra_jobs:
        jobs.append(run_job(root,name,overrides=options,hook_options=hooks))
    return conversions,modifications,jobs


def offset_cases():
    cases=[]
    for label,value in [('none',None),('invalid',dt.NONE),('lower','0001-01-01'),('upper','9999-12-31T23:59:59')]:
        for offset in ('+02:00','-01:00'):
            for conv in ('D','d') if label in ('none','invalid') else ('D',):
                prefix='{value!'+conv+':O'+offset+'/'
                for suffix in ('','CT/','Cs/','Cr/','Cj/','I/'):
                    template=prefix+suffix+'}'
                    cases.append({'name':label+'_'+offset+'_'+conv+'_'+suffix,'context':encode({'value':value}),
                                  'template':template,**evaluate({'value':value},template)})
                options={'mode':'modify','fields':{'copied':prefix+'I/}',
                         'year':'{copied.year}','month':'{copied.month}','day':'{copied.day}','hour':'{copied.hour}',
                         'epoch':'{copied!T}','string':'{copied!s}','repr':'{copied!r}','json':'{copied!j}',
                         'formatted':'{copied:%Y-%m-%d %H:%M:%S}'}}
                data={'value':value}
                metadata.MetadataPP(ModifyJob(),options).run(SimpleNamespace(kwdict=data))
                cases.append({'name':label+'_'+offset+'_'+conv+'_modify','context':encode({'value':value}),
                              'options':options,'output':encode(data)})
    return cases


if __name__=='__main__':
    conversions,modifications,jobs=build()
    files={'formatter':Path(inspect.getfile(formatter)),'dt':Path(inspect.getfile(dt)),
           'metadata':Path(inspect.getfile(metadata)),'job':Path(inspect.getfile(job)),
           'bound_inputs':Path(bounds.__file__),'job_runner':Path(defaults.__file__)}
    result={'source':'gallery-dl','version':'1.32.17','source_sha256':{key:hashlib.sha256(value.read_bytes()).hexdigest() for key,value in files.items()},
            'conversion_cases':conversions,'modify_cases':modifications,'job_cases':jobs,'offset_cases':offset_cases()}
    Path(__file__).with_name('formatter_dates.json').write_text(json.dumps(result,ensure_ascii=True,indent=2)+'\n')
