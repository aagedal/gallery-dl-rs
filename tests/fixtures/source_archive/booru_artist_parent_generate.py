#!/usr/bin/env python3
"""Actual source artist parent-metadata modes and keyword ownership.

Imports the frozen artist runner unchanged: real DownloadJob.handle_queue,
config2, predicates, PathFormat, MetadataPP and SQLite run. Only response and
source download bytes are controlled. No inheritance algorithm is copied here.
Run with gallery-dl 1.32.17 Python -B.
"""
import copy
import hashlib
import inspect
import json
from pathlib import Path
from gallery_dl import job
from gallery_dl.postprocessor import metadata
import booru_artist_generate as artist


def modify(event, fields):
    return {'name':'metadata','mode':'modify','event':event,'fields':fields}


def run(family,name,**kwargs):
    actual_types=[]
    original=metadata.MetadataPP._run_modify
    original_skip=job.DownloadJob.handle_skip
    def observed_skip(self):
        self.observe('skip',self.pathfmt)
        return original_skip(self)
    def observed(self,pathfmt):
        original(self,pathfmt)
        for key in ('raw','copy'):
            if key in pathfmt.kwdict:
                value=pathfmt.kwdict[key]
                actual_types.append({'field':key,'type':type(value).__name__,'str':str(value)})
    metadata.MetadataPP._run_modify=observed
    job.DownloadJob.handle_skip=observed_skip
    try:result=artist.run(family,name,**kwargs)
    finally:
        metadata.MetadataPP._run_modify=original
        job.DownloadJob.handle_skip=original_skip
    if actual_types:result['actual_types']=actual_types
    return result


def build():
    cases=[]
    for family in ('danbooru','e621','e926'):
        values=[('null',None),('false',False),('zero',0),('empty_string',''),('empty_array',[]),('empty_object',{}),
                ('custom','artist_data'),('true',True),('integer',2),('array',['x']),('object',{'x':1})]
        for name,value in values:
            cases.append(run(family,'primary_'+name,options={'parent-metadata':value}))
            cases.append(run(family,'legacy_'+name,options={'metadata-parent':value}))
        for name,value in [('null',None),('false',False),('zero',0),('empty_string',''),('custom','primary'),('true',True)]:
            cases.append(run(family,'primary_presence_'+name,options={'parent-metadata':value,'metadata-parent':'legacy'}))
        for key in ('date','metadata','_extractor','_parent'):
            cases.append(run(family,'custom_key_'+key,options={'parent-metadata':key}))
        collision={'id':99,'name':'Cat','date':'2024-01-02T03:04:05+00:00','date_updated':'2023-02-03 04:05:06',
                   'date_url':'2022-03-04T05:06:07','category':'API_CATEGORY','subcategory':'API_SUBCATEGORY',
                   'search_tags':'PARENT_TAGS','filename':'PARENT_FILENAME','extension':'jpeg','tags':['PARENT_TAG'],
                   'nested':{'x':0},'replacement':{'x':100},'primitives':[None,False,0,1.5,'']}
        for mode in (False,'artist_data',True):
            label={False:'disabled',True:'flat'}.get(mode,'custom')
            cases.append(run(family,'collision_'+label,responses=[collision],options={'parent-metadata':mode},child_pages=[[42,41]]))
        cases.append(run(family,'flat_parent_num',responses=[dict(collision,num=7)],options={'parent-metadata':True}))
        for event in ('post','file','post-after'):
            cases.append(run(family,'flat_reapply_'+event,responses=[collision],options={'init':True,'parent-metadata':True},child_pages=[[42,41]],
                child_hook_options=[modify(event,{'id':'777','name':'HOOK','date':'HOOK_DATE','search_tags':'HOOK_TAGS','filename':'HOOK_FILENAME','extension':'gif','tags':'HOOK_TAGS','num':'99'})]))
        for mode in (True,'artist_data'):
            label='flat' if mode is True else 'custom'
            def field(name):return name if mode is True else 'artist_data['+name+']'
            for event in ('post','file'):
                for kind,fields in [('scalar',{field('name'):'EDIT'}),
                                    ('nested',{field('nested')+'[x]':'{'+field('nested')+'[x]!i:A+1/}'}),
                                    ('replace',{field('nested'):'{'+field('replacement')+'}'})]:
                    rows=[{'id':1,'name':'Cat','nested':{'x':0},'replacement':{'x':100}},
                          {'id':2,'name':'Dog','nested':{'x':10},'replacement':{'x':200}}]
                    cases.append(run(family,label+'_'+kind+'_'+event,responses=[rows],route='/artists?x=1',
                        options={'init':True,'parent-metadata':mode},child_pages=[[42,41],[40,39]],child_hook_options=[modify(event,fields)]))
        for mode in (False,'artist_data',True):
            label='flat' if mode is True else 'custom' if mode else 'disabled'
            cases.append(run(family,'child_hook_before_'+label,responses=[collision],options={'init':True,'parent-metadata':mode,'child-filter':"name == 'Cat'"},
                hooks=[modify('child',{'id':'77','name':'CHILD','nested[x]':'1','search_tags':'CHILD_TAGS','_extractor':'{_extractor}'})]))
        for mode,key in [('artist_data','artist_data'),(True,None)]:
            source='date' if key is None else key+'[date]'
            cases.append(run(family,'raw_date_'+('flat' if key is None else 'custom'),responses=[collision],options={'parent-metadata':mode},
                child_hook_options=[modify('post',{'raw':'{'+source+'}','copy':'{'+source+'!D}'}),
                    {'name':'metadata','mode':'custom','event':'file','filename':'date-copy.txt','format':'{raw!u}|{copy:%Y}'}]))
    return cases


if __name__=='__main__':
    files={'artist_runner':Path(artist.__file__),'job':Path(inspect.getfile(job)),'metadata':Path(inspect.getfile(metadata))}
    result={'source':'gallery-dl','version':'1.32.17','source_sha256':{key:hashlib.sha256(path.read_bytes()).hexdigest() for key,path in files.items()},
            'normalization':{'trace_omitted_keys':['_path','_extr'],'classes':'actual class __name__','datetime':'actual str(datetime); explicit identity types separately recorded'},'cases':build()}
    Path(__file__).with_name('booru_artist_parent.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
