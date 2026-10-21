#!/usr/bin/env python3
"""Actual gallery-dl PathFormat result-type behavior via real DownloadJobs.

Imports frozen source-only runners, replaces neither formatting nor paths.
All values enter a controlled raw API post. Only response/download bytes are
controlled by the imported source runner; typed callbacks are actual runtime.
"""
import hashlib
import json
import sys
from tempfile import TemporaryDirectory
from gallery_dl import config,extractor,path as source_path,formatter,job,util
import inspect
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'postprocessors'))
from formatter_dates_generate import run_job,encode
import booru_defaults_generate as defaults

def build():
    root=next(item for item in defaults.ROOTS if item[0]=='danbooru_post')
    cases=[]
    inputs=[('date_D','2024-01-02T03:04:05','{raw!D}'),('invalid_D',None,'{raw!D}'),('date_d',0,'{raw!d}'),
            ('invalid_d',None,'{raw!d}'),('date_formatted','2024-01-02T03:04:05','{raw!D:%Y}'),
            ('string','Cat','{raw:I/}'),('integer',7,'{raw:I/}'),('zero',0,'{raw:I/}'),
            ('float',1.5,'{raw:I/}'),('boolean',True,'{raw:I/}'),('none',None,'{raw:I/}'),
            ('list',['a','b'],'{raw:I/}'),('empty_list',[],'{raw:I/}'),('nested_list',[['a'],['b']],'{raw:I/}'),
            ('mapping',{'a':1},'{raw:I/}'),('empty_string','','{raw:I/}')]
    for name,value,template in inputs:
        for target in ('filename','directory'):
            options={'filename':'control.{extension}','directory':['control']}
            if target=='filename':options['filename']=template
            else:options['directory']=['result',template]
            case=run_job(root,name+'_'+target,overrides=options,input_overrides={'raw':value})
            cases.append({'name':name+'_'+target,'target':target,'raw_value':value,'template':template,'case':case})
    for label,value,template in [('date','2024-01-02T03:04:05','before{raw!D}after'),('list',['a','b'],'before{raw:I/}after'),
                                 ('number',7,'before{raw:I/}after'),('formatted_date','2024-01-02T03:04:05','before{raw!D:%Y}after')]:
        for target in ('filename','directory'):
            options={'filename':'control.{extension}','directory':['control']}
            if target=='filename':options['filename']=template
            else:options['directory']=['result',template]
            cases.append({'name':'mixed_'+label+'_'+target,'target':target,'raw_value':value,'template':template,
                          'case':run_job(root,'mixed_'+label+'_'+target,overrides=options,input_overrides={'raw':value})})
    for label,value,template in [('default_number',7,'{raw}'),('default_list',['a','b'],'{raw}'),
                                 ('default_typed_date',None,'{date}'),('identity_typed_date',None,'{date:I/}')]:
        for target in ('filename','directory'):
            options={'filename':'control.{extension}','directory':['control']}
            if target=='filename':options['filename']=template
            else:options['directory']=['result',template]
            cases.append({'name':label+'_'+target,'target':target,'raw_value':value,'template':template,
                          'case':run_job(root,label+'_'+target,overrides=options,input_overrides={'raw':value})})
    extra=[('missing_identity',None,'{missing:I/}'),('empty_field',None,'{}'),
           ('empty_D',None,'{!D}'),('empty_d',None,'{!d}'),
           ('integer_conversion','12','{raw!i}'),('float_conversion','1.5','{raw!f}'),
           ('integer_formatted','12','{raw!i:03}'),('float_formatted','1.5','{raw!f:.1f}'),
           ('integer_bad','PRIVATE_BAD_NUMBER','{raw!i}'),('float_bad','PRIVATE_BAD_NUMBER','{raw!f}')]
    for label,value,template in extra:
        for target in ('filename','directory'):
            options={'filename':'control.{extension}','directory':['control']}
            if target=='filename':options['filename']=template
            else:options['directory']=['result',template]
            for empty_key in ([False,True] if label.startswith('empty_') else [False]):
                changes={'raw':value}
                if empty_key:changes['']='2024-01-02T03:04:05'
                name=label+'_'+target+('_present_empty_key' if empty_key else '')
                cases.append({'name':name,'target':target,'raw_value':value,'template':template,'empty_context_key':empty_key,
                              'case':run_job(root,name,overrides=options,input_overrides=changes)})
    return cases
def actual_empty_open_cases():
    cases=[]
    for label,template,data in [('empty_raw','{raw:I/}',{'raw':''}),('empty_field','{}',{'':'IGNORED'})]:
        for part in (False,True):
            config.clear()
            with TemporaryDirectory(prefix='path-results-open-',dir='/private/tmp') as directory:
                obj=extractor.find('https://danbooru.donmai.us/posts/42')
                for key,value in {'base-directory':directory,'directory':['control'],'filename':template}.items():
                    config.set(('extractor','danbooru'),key,value)
                p=source_path.PathFormat(obj)
                keywords={'id':42,'extension':'jpg',**data}
                p.set_directory(keywords);p.set_filename(keywords);p.build_path()
                def relative(value):return str(value).replace(directory+'/', '').replace(directory,'$BASE')
                item={'name':label,'template':template,'keywords':data,'part_enabled':part,
                      'filename':p.filename,'path':relative(p.path),'realpath':relative(p.realpath),'temppath_before_part':relative(p.temppath)}
                if part:p.part_enable()
                item['temppath_at_open']=relative(p.temppath)
                try:
                    with p.open('w+b') as fp:fp.write(b'actual-source-open')
                    item['open']='success'
                except Exception as exc:item['open_error']={'class':type(exc).__name__,'message':relative(str(exc))}
                if item.get('open')=='success':
                    try:item['finalize_result']=p.finalize()
                    except Exception as exc:item['finalize_error']={'class':type(exc).__name__,'message':relative(str(exc))}
                item['remaining_files']=[{'path':str(file.relative_to(directory)),'data_hex':file.read_bytes().hex()} for file in sorted(Path(directory).rglob('*')) if file.is_file()]
                cases.append(item)
            config.clear()
    return cases

def prepare_cases():
    root=next(item for item in defaults.ROOTS if item[0]=='danbooru_post')
    specifications=[
        ('prepare_repairs_typed_filename',7,'{raw:I/}',
         [{'name':'metadata','mode':'modify','event':'prepare','fields':{'raw':'fixed'}}]),
        ('prepare_breaks_valid_filename','valid','{raw:I/}',
         [{'name':'metadata','mode':'modify','event':'prepare','fields':{'raw':'{number}'}}]),
        ('prepare_default_metadata_valid','valid','control.{extension}',
         [{'name':'metadata','event':'prepare','include':['raw','filename','extension']}]),
        ('prepare_default_metadata_before_failure',7,'{raw:I/}',
         [{'name':'metadata','event':'prepare','include':['raw','filename','extension']}]),
        ('prepare_hash_first_file','valid','control.{extension}',
         [{'name':'hash','event':'prepare','hashes':'sha256'}]),
        ('prepare_provisional_path_not_hash_input','valid','control.{extension}',
         [{'name':'metadata','mode':'custom','event':'prepare','filename':'control.jpg','format':'EXISTING_SOURCE_INPUT'},
          {'name':'hash','event':'prepare','hashes':'sha256'}]),
    ]
    return [{'name':name,'raw_value':raw,'template':template,
             'case':run_job(root,name,overrides={'directory':['control'],'filename':template},
                            hook_options=hooks,input_overrides={'raw':raw,'number':7})}
            for name,raw,template,hooks in specifications]


def conversion_failure_cases():
    cases=[]
    for name,raw,template in [('bad_i','PRIVATE_BAD_INT','{raw!i:03}'),
                              ('bad_f','PRIVATE_BAD_FLOAT','{raw!f:.2f}'),
                              ('numeric_lower',1,'{raw!l:>8}'),
                              ('bad_chain_i','PRIVATE_BAD_INT','{raw:Ci/03}'),
                              ('bad_chain_f','PRIVATE_BAD_FLOAT','{raw:Cf/.2f}')]:
        entry={'name':name,'context':{'raw':raw},'template':template}
        for mode,args in [('default',()),('identity',(None,util.identity))]:
            try:entry[mode]={'output':encode(formatter.parse(template,*args).format_map(entry['context']))}
            except Exception as exc:entry[mode]={'error':type(exc).__name__,'message':str(exc)}
        cases.append(entry)
    return cases

if __name__=='__main__':
    cases=build()
    for row in cases:
        file_paths=[{key:event[key] for key in ('filename','path','realpath','temppath')} for event in row['case']['trace'] if event['event']=='file']
        row['exact_pathformat_results']=file_paths
        # Recreate the exact emitted source keyword context from its genuine
        # input API post using the real source items() for independent direct
        # formatter observation. No format result or path algorithm is copied.
        obj=extractor.find(row['case']['url'])
        obj._get_auth_info=lambda:(None,None)
        obj.ugoira=False;obj.external=False;obj.includes=False
        obj.request_json=lambda *args,**kwargs:row['case']['input_posts'][0].copy()
        kwargs=next(obj.items())[2]
        try:
            result=formatter.parse(row['template']).format_map(kwargs)
            row['direct_formatter']={'type':type(result).__name__,'text':str(result),'typed':encode(result)}
            try:row['direct_formatter']['json']=json.dumps(result,default=util.json_default)
            except Exception as exc:row['direct_formatter']['json_error']=type(exc).__name__
        except Exception as exc:row['direct_formatter']={'error':type(exc).__name__,'message':str(exc)}
        row['empty_basename_transfer_caveat']=any(not item['filename'] for item in file_paths)
    result={'source':'gallery-dl','version':'1.32.17',
            'transfer_caveat':'The imported job runner writes through pathlib.Path, which normalizes trailing separators. Empty-basename published artifacts are synthetic-transfer observations, not evidence that Source HTTP can publish them. Exact unnormalized PathFormat paths remain in trace/exact_pathformat_results. actual_empty_open_cases uses real PathFormat.open/part_enable/finalize without network.',
            'cases':cases,'actual_empty_open_cases':actual_empty_open_cases(),
            'prepare_cases':prepare_cases(),'conversion_failure_cases':conversion_failure_cases(),
            'source_sha256':{name:hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest() for name,module in {'formatter':formatter,'path':source_path,'job':job,'defaults_runner':defaults,'typed_runner':sys.modules[run_job.__module__],'hash':defaults.hash_pp}.items()}}
    Path(__file__).with_name('booru_path_results.json').write_text(json.dumps(result,ensure_ascii=True,indent=2)+'\n')
    for row in result['cases']:print(row['name'],row['case']['error'],[item['path'] for item in row['case']['published']])
