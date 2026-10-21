#!/usr/bin/env python3
"""Offline gallery-dl 1.32.17 artists oracle: real Queue/DownloadJob children.

Only HTTP response/download bytes are replaced. Real constructors, items,
request_json, predicates, visited set, handle_queue, PathFormat, MetadataPP,
archives and publication run. Observers do not reconstruct child traversal.
Run with the installed gallery-dl Python and -B.
"""
import copy
import collections
import datetime
import hashlib
import inspect
import json
import sqlite3
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from urllib.parse import parse_qsl, urlsplit
from gallery_dl import config, extractor, job, exception
from gallery_dl.extractor import common, danbooru, e621
from booru_search_generate import input_post

BODY = b"source-artist-fixture"

def run(family, name, responses=None, route='/artists/0007tail', options=None,
        hooks=None, child_pages=None, consume_limit=None, child_hook_options=None):
    config.clear()
    host={'danbooru':'danbooru.donmai.us','e621':'e621.net','e926':'e926.net'}[family]
    url='https://'+host+route
    responses=copy.deepcopy(responses if responses is not None else [{'id':99,'name':'Cat','other_names':['cat_alias'],'nested':{'public':True}}])
    child_pages=copy.deepcopy(child_pages if child_pages is not None else [[42]])
    calls=[];trace=[];queues=[];errors=[];ai=ci=0
    with TemporaryDirectory(prefix='booru-artists-') as temp:
        base=Path(temp);obj=extractor.find(url)
        if obj is None:return {'name':family+'_'+name,'family':family,'url':url,'extractor':None}
        root_subcat=obj.subcategory
        for key,value in {'base-directory':temp,'async':False,'sleep':0,'sleep-skip':0}.items():
            config.set(('extractor',family),key,value)
        for key,value in (options or {}).items():config.set(('extractor',family,root_subcat),key,value)
        if hooks:config.set(('extractor',family,root_subcat),'postprocessors',copy.deepcopy(hooks))
        child_observer={'name':'metadata','mode':'jsonl','event':'file','filename':'files.jsonl','base-directory':True,'directory':[],
                        'archive':[':basedirectory','metadata.sqlite'],
                        'include':['category','subcategory','id','search_tags','filename','extension']}
        config.set(('extractor',family,'tag'),'postprocessors',copy.deepcopy(child_hook_options or [])+[child_observer])
        def snap(value):
            if isinstance(value,dict):return {k:snap(v) for k,v in value.items() if k not in ('_path','_extr')}
            if isinstance(value,(tuple,list)):return [snap(v) for v in value]
            if isinstance(value,type):return value.__name__
            if isinstance(value,datetime.datetime):return str(value)
            return value
        def rel(value):return str(value).replace(temp+'/', '').replace(temp,'$BASE') if value else ''
        def transport(self, request_url, params=None, **kwargs):
            nonlocal ai,ci
            wire=dict(parse_qsl(urlsplit(request_url).query,keep_blank_values=True));wire.update({k:v for k,v in (params or {}).items() if v is not None})
            calls.append({'url':request_url,'params':copy.deepcopy(params),'wire_params':wire})
            if len(calls)>100:raise RuntimeError('OracleRequestBoundExceeded')
            if '/artists' in urlsplit(request_url).path:
                if ai>=len(responses):raise RuntimeError('UnexpectedArtistRequest')
                body=responses[ai];ai+=1
            else:
                if ci>=len(child_pages):raise RuntimeError('UnexpectedTagRequest')
                page=child_pages[ci];ci+=1
                rows=[dict(input_post('danbooru' if family=='danbooru' else 'e621'),id=pid) for pid in page]
                body=rows if family=='danbooru' else {'posts':rows}
            if isinstance(body,dict) and '$error' in body:
                calls[-1]['transport_error']=body['$error']
                raise RuntimeError(body['$error'])
            calls[-1]['response']=copy.deepcopy(body)
            calls[-1]['status']=200
            return SimpleNamespace(text=json.dumps(body,ensure_ascii=False))
        class Actual(job.DownloadJob):
            def initialize(self,data=None):
                super().initialize(data)
                if not self.hooks:self.hooks=collections.defaultdict(list)
                for event in ('init','post','prepare','file','after','post-after','child','child-after','finalize'):
                    self.register_hooks({event:lambda p,event=event:self.observe(event,p)})
                trace.append({'event':'initialized','subcategory':self.extractor.subcategory,'metadata':snap(self.pathfmt.kwdict)})
            def observe(self,event,p):
                trace.append({'event':event,'subcategory':self.extractor.subcategory,'metadata':snap(p.kwdict),
                              'directory':rel(p.directory),'filename':p.filename,'path':rel(p.path)})
            def download(self,request_url):
                p=self.pathfmt
                if not p.temppath:p.fix_extension()
                target=Path(p.temppath);target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes(BODY);return True
            def dispatch(self,messages):
                def observed():
                    count=0
                    if self.extractor is obj and consume_limit==0:return
                    try:
                        for kind,message_url,data in messages:
                            if kind==common.Message.Queue:
                                queues.append({'url':message_url,'metadata':snap(data)})
                            yield kind,message_url,data
                            if self.extractor is obj and kind==common.Message.Queue:
                                count+=1
                                if consume_limit is not None and count>=consume_limit:return
                    except exception.StopExtraction:
                        raise
                    except Exception as exc:
                        errors.append({'subcategory':self.extractor.subcategory,'class':type(exc).__name__,'message':str(exc)})
                        raise
                return super().dispatch(observed())
            def handle_queue(self,queue_url,data):
                trace.append({'event':'queue-selected','subcategory':self.extractor.subcategory,'url':queue_url,'metadata':snap(data),'already_visited':queue_url in self.visited})
                return super().handle_queue(queue_url,data)
        original_request=common.Extractor.request
        original_auth=danbooru.DanbooruExtractor._get_auth_info
        original_store=common.Extractor.cookies_store
        common.Extractor.request=transport
        danbooru.DanbooruExtractor._get_auth_info=lambda self:(None,None)
        common.Extractor.cookies_store=lambda self:None
        error=None
        try:
            current=Actual(obj);current.out=SimpleNamespace(success=lambda p:None,skip=lambda p:None)
            current.run()
            root_errors=[item for item in errors if item['subcategory']==root_subcat]
            error=copy.deepcopy(root_errors[0]) if root_errors else None
            status=current.status
        finally:
            common.Extractor.request=original_request
            danbooru.DanbooruExtractor._get_auth_info=original_auth
            common.Extractor.cookies_store=original_store
        published=[];sql=[];databases=[]
        for file in sorted(base.rglob('*')):
            if not file.is_file():continue
            if file.suffix=='.sqlite':
                con=sqlite3.connect(file);entries=[row[0] for row in con.execute('SELECT entry FROM archive ORDER BY entry')];sql.extend(entries);databases.append({'path':str(file.relative_to(base)),'entries':entries});con.close()
            else:published.append({'path':str(file.relative_to(base)),'data_hex':file.read_bytes().hex()})
        result={'name':family+'_'+name,'family':family,'url':url,'extractor':type(obj).__name__,'subcategory':root_subcat,
                'archive_format':obj.archive_fmt,'directory_format':list(obj.directory_fmt),'filename_format':obj.filename_fmt,
                'options':options or {},'hook_options':hooks or [],'artist_responses':responses,'child_pages':child_pages,
                'consume_limit':consume_limit,'download_body':BODY.decode(),'calls':calls,'queue_events':queues,'trace':trace,
                'published':published,'archive_sql':sorted(sql),'archive_databases':databases,'error':error,'source_errors':errors,'status':status,
                'unconsumed_artist_responses':len(responses)-ai,'unconsumed_child_pages':len(child_pages)-ci}
    if child_hook_options is not None:result['child_hook_options']=child_hook_options
    config.clear();return result

def build():
    cases=[]
    def mod(event,fields):return {'name':'metadata','mode':'modify','event':event,'fields':fields}
    for family in ('danbooru','e621','e926'):
        for name,artist in [('detail',{'id':99,'name':'Cat','other_names':['alias'],'nested':{'list':[1,None,True]},'category':'API','subcategory':'API','_extractor':'API'}),
                            ('quoted',{'id':99,'name':'A B+C/D%雪'}),('empty_name',{'name':''}),('missing_name',{'id':99}),('null_name',{'name':None}),('numeric_name',{'name':4}),('array_detail',[])]:
            cases.append(run(family,name,[artist]))
        cases.append(run(family,'search_two_pages',[[{'id':i,'name':f'Cat{i}'} for i in range(20)],[{'name':'Tail'}]],route='/artists/?search[name]=cat&limit=1',child_pages=[[42]]*21))
        for name,page in [('empty_array',[]),('empty_object',{}),('empty_string',''),('null',None),('wrapped',{'artists':[]}),('partial_bad_row',[{'name':'Good'},None,{'name':'Later'}]),('partial_missing_name',[{'name':'Good'},{}])]:
            cases.append(run(family,'search_'+name,[page],route='/artists?search[name]=cat'))
        for value in ('','0','-2','%20%2B%EF%BC%93%20','1_0','1.2','bad'):
            cases.append(run(family,'page_'+value,[[]],route='/artists?page='+value+'&limit=7'))
        cases.append(run(family,'query_first_keys',[[]],route='/artists?page&page=-2&page=88&%6cimit=1&limit=9&search%5Bname%5D=A%2BB+cat&blank='))
        cases.append(run(family,'filter_reject',[{'name':'Cat'}],options={'child-filter':'False'}))
        cases.append(run(family,'malformed_before_filter',[{'id':99}],options={'child-filter':'False'}))
        cases.append(run(family,'range_zero',[[{'name':'Cat'},{'name':'Dog'}]],route='/artists?x=1',options={'child-range':'0'}))
        cases.append(run(family,'duplicates',[[{'name':'Cat'},{'name':'Cat'},{'name':'Dog'}]],route='/artists?x=1',child_pages=[[42],[41]]))
        cases.append(run(family,'filter_before_dedup',[[{'id':1,'name':'Cat'},{'id':2,'name':'Cat'}]],route='/artists?x=1',options={'child-filter':'id == 2'}))
        for n in (0,1):cases.append(run(family,'cancel_'+str(n),[[{'name':'Cat'} for _ in range(20)],[]],route='/artists?x=1',consume_limit=n))
        cases.append(run(family,'eager_child_mutations',[[{'id':1,'name':'Cat','nested':{'public':True}},{'id':2,'name':'Dog'}]],route='/artists?x=1',child_pages=[[42],[41]],options={'init':True},hooks=[mod('child',{'name':'HOOK','marker':'CHILD'}),mod('child-after',{'name':'AFTER','marker':'AFTER'})]))
        cases.append(run(family,'late_transport_failure',[[{'name':str(i)} for i in range(20)],{'$error':'SyntheticArtistFailure'}],route='/artists?x=1',child_pages=[[42]]*20))
        cases.append(run(family,'range_second',[[{'name':'Cat'},{'name':'Dog'}]],route='/artists?x=1',options={'child-range':'2'},child_pages=[[41]]))
        cases.append(run(family,'lazy_child_hooks_absent',[{'name':'Cat'}],hooks=[mod('child',{'marker':'SHOULD_NOT_RUN'})]))
        cases.append(run(family,'eager_empty_root_archive',[{'name':'Cat'}],options={'init':True,'archive':[':basedirectory','root.sqlite']}))
    # Appended cases: real MetadataPP traversal tests shallow parent inheritance.
    for family in ('danbooru','e621','e926'):
        artists=[{'id':1,'name':'Cat','nested':{'x':0},'replacement':{'x':100},'replacement_parent':{'name':'REPLACED','nested':{'x':200}}},
                 {'id':2,'name':'Dog','nested':{'x':10},'replacement':{'x':300},'replacement_parent':{'name':'SECOND','nested':{'x':400}}}]
        mutations=[
            ('parent_name_post',[mod('post',{'_parent[name]':'POST'})]),
            ('parent_name_file',[mod('file',{'_parent[name]':'FILE'})]),
            ('parent_nested_post',[mod('post',{'_parent[nested][x]':'{_parent[nested][x]!i:A+1/}'})]),
            ('parent_nested_file',[mod('file',{'_parent[nested][x]':'{_parent[nested][x]!i:A+1/}'})]),
            ('parent_nested_replaced',[mod('post',{'_parent[nested]':'{_parent[replacement]}'}),mod('file',{'_parent[nested][x]':'{_parent[nested][x]!i:A+1/}'})]),
            ('parent_root_replaced_file',[mod('file',{'_parent':'{_parent[replacement_parent]}'}),mod('file',{'_parent[nested][x]':'{_parent[nested][x]!i:A+1/}'})]),
            ('parent_root_replaced',[mod('post',{'_parent':'{_parent[replacement_parent]}'}),mod('file',{'_parent[nested][x]':'{_parent[nested][x]!i:A+1/}'})]),
        ]
        for name,child_hooks in mutations:
            cases.append(run(family,name,[artists],route='/artists?x=1',options={'init':True},
                             child_pages=[[42,41],[40,39]],child_hook_options=child_hooks))
        raw={'id':7,'name':'Dates','date':'2024-01-02T03:04:05','date_updated':'2023-02-03 04:05:06',
             'date_url':'2022-03-04T05:06:07','nested':{'date':'2021-04-05T06:07:08'},'primitive':[None,False,0,1.5,'']}
        date_hook={'name':'metadata','mode':'custom','event':'file','filename':'date-types.txt','format':
                   '{_parent[date]!u}|{_parent[date_updated]!u}|{_parent[date_url]!u}|{_parent[nested][date]!u}|{_parent[date]!D:%Y}'}
        cases.append(run(family,'parent_raw_date_strings',[raw],child_hook_options=[date_hook]))
    return cases

if __name__=='__main__':
    sources={'danbooru':Path(inspect.getfile(danbooru)),'e621':Path(inspect.getfile(e621)),'job':Path(inspect.getfile(job))}
    route_cases=[]
    for host in ('danbooru.donmai.us','e621.net','e926.net'):
        for route in ('/artists/7','/artists/0007tail','/artists/0','/artists?x=1','/artists/?x=1','/artists?','/artists?bare#fragment','/artists/7?x=1'):
            url='https://'+host+route
            item=extractor.find(url)
            route_cases.append({'url':url,'extractor':type(item).__name__ if item else None,'category':item.category if item else None,'subcategory':item.subcategory if item else None,'groups':list(item.groups) if item else None})
    result={'normalization':{'trace_omitted_keys':['_path','_extr'],'classes':'actual class __name__','datetime':'actual str(datetime)','published_bytes':'unchanged real output; observer includes supported scalar file fields, full parent retained in trace'},'route_cases':route_cases,'source':'gallery-dl','version':'1.32.17','source_sha256':{k:hashlib.sha256(p.read_bytes()).hexdigest() for k,p in sources.items()},'cases':build()}
    Path(__file__).with_name('booru_artist.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
