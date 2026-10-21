#!/usr/bin/env python3
"""Genuine source date predicates on live booru job dictionaries.

Reuses frozen artist/Popular real DownloadJob runners. Only observers wrap
predicate_date and consumed Job messages; the predicate implementation and
selection order remain authoritative source. Artist options are set on actual
child tag config, not emulated on a reconstructed child dictionary. Helper
vectors explicitly distinguish artificial skip callbacks from inherited absent skip callback.
"""
import contextlib
import copy
import hashlib
import inspect
import io
import json
from pathlib import Path
from gallery_dl import config, util, job, dt
from gallery_dl.extractor import common
import booru_artist_generate as artists
import booru_popular_generate as popular


def run(family,name,selection,row=None,mode=True,is_popular=False,child_pages=None,tag_selection=None):
    calls=[];messages=[];constructions=[];current_kind=None
    original_date=util.predicate_date
    original_dispatch=job.Job.dispatch
    original_set=config.set
    original_today=dt.date
    def observed_date(before,after=None,skip=None):
        entry={'before':str(before) if before is not None else None,'after':str(after) if after is not None else None,
               'skip_supplied':skip is not None,'skip_calls':[]}
        constructions.append(entry)
        def observed_skip(bound):
            result=skip(bound)
            entry['skip_calls'].append({'bound':str(bound),'result':result})
            return result
        predicate=original_date(before,after,observed_skip if skip is not None else None)
        def observed(url,data):
            value=data.get('date')
            item={'message':current_kind,'url':url,'date_present':'date' in data,'value_type':type(value).__name__,
                  'value_str':str(value),'truthy':bool(value)}
            calls.append(item)
            try:
                result=predicate(url,data);item['result']=result;return result
            except Exception as exc:
                item['exception']={'class':type(exc).__name__,'message':str(exc)}
                raise
        return observed
    def observed_dispatch(self,source):
        def consumed():
            nonlocal current_kind
            for kind,url,data in source:
                current_kind={common.Message.Directory:'post',common.Message.Url:'media',common.Message.Queue:'child'}.get(kind,str(kind))
                messages.append({'subcategory':self.extractor.subcategory,'kind':current_kind,'url':url})
                yield kind,url,data
        return original_dispatch(self,consumed())
    def configured(path,key,value):
        original_set(path,key,value)
        if not is_popular and path in (('extractor',family,'artist'),('extractor',family,'artist-search')) and key in selection:
            if tag_selection is None:
                original_set(('extractor',family,'tag'),key,value)
            else:
                for child_key,child_value in tag_selection.items():
                    original_set(('extractor',family,'tag'),child_key,child_value)
    util.predicate_date=observed_date
    job.Job.dispatch=observed_dispatch
    config.set=configured
    dt.date=popular.FrozenDate
    try:
        with contextlib.redirect_stdout(io.StringIO()):
            if is_popular:
                case=popular.run(family,name,options=selection)
            else:
                case=artists.run(family,name,responses=[copy.deepcopy(row or {'name':'Cat'})],
                    options={'parent-metadata':mode,**selection},child_pages=child_pages,
                    route='/artists?x=1' if isinstance(row,list) else '/artists/0007tail')
    finally:
        util.predicate_date=original_date
        job.Job.dispatch=original_dispatch
        config.set=original_set
        dt.date=original_today
    result={'name':family+'_'+name,'kind':'popular' if is_popular else 'artist','selection':selection,
            'inheritance':mode if not is_popular else None,'case':case,'consumed_messages':messages,
            'predicate_constructions':constructions,'date_predicate_calls':calls}
    if tag_selection is not None:result['tag_selection']=tag_selection
    return result


def helper_cases():
    result=[]
    for skip_value in (None,False,True):
        for before,after in [('2024-01-02T03:04:05',None),(None,'2024-01-02T03:04:05'),('2025-01-01','2023-01-01'),('2023-01-01','2025-01-01')]:
            observed=[]
            def skip(value):observed.append(str(value));return skip_value
            pred=util.predicate_date(dt.convert(before) if before else None,dt.convert(after) if after else None,skip)
            outcomes=[]
            for date in ('2024-01-02T03:04:04','2024-01-02T03:04:05','2024-01-02T03:04:06'):
                try:outcomes.append({'date':date,'result':pred('',{'date':dt.convert(date)})})
                except Exception as exc:outcomes.append({'date':date,'exception':type(exc).__name__})
            result.append({'before':before,'after':after,'artificial_skip_result':skip_value,'skip_calls':observed,'outcomes':outcomes})
    return result


def build():
    cases=[]
    for family in ('danbooru','e621','e926'):
        for name,value in [('iso','2024-01-02T03:04:05+00:00'),('bad','PRIVATE_INVALID_DATE'),('empty',''),('null',None),('zero',0),('false',False),('true',True),('number',1),('empty_array',[]),('array',[1]),('empty_object',{}),('object',{'x':1})]:
            cases.append(run(family,'flat_'+name,{'date-before':'2025-01-01'},row={'name':'Cat','date':value}))
        cases.append(run(family,'flat_missing',{'date-before':'2025-01-01'}))
        for name,selection,mode in [
            ('filter_rejects_before_bad_date',{'date-after':'2025-01-01','post-filter':'False'},True),
            ('range_zero_does_not_hide_bad_date',{'date-after':'2025-01-01','post-range':'0'},True),
            ('bad_before_disables',{'date-before':'PRIVATE_INVALID_BOUND'},True),
            ('bad_after_disables',{'date-after':'PRIVATE_INVALID_BOUND'},True),
            ('zero_bound_disables',{'date-after':0},True),
            ('false_bound_disables',{'date-before':False},True),
            ('null_bound_disables',{'date-after':None},True),
            ('blank_bound_disables',{'date-before':''},True),
            ('nested_retains_child_datetime',{'date-before':'2025-01-01'},'_parent'),
            ('before_equal_rejects',{'date-before':'2024-01-02T03:04:05'},False),
            ('after_equal_stops',{'date-after':'2024-01-02T03:04:05'},False),
            ('equal_bounds_accept_nothing',{'date-before':'2024-01-02T03:04:05','date-after':'2024-01-02T03:04:05'},False),
            ('before_one_second_later_accepts',{'date-before':'2024-01-02T03:04:06'},False),
            ('after_one_second_earlier_accepts',{'date-after':'2024-01-02T03:04:04'},False),
            ('valid_window',{'date-after':'2023-01-01','date-before':'2025-01-01'},False),
            ('inverted_window_after_only',{'date-after':'2025-01-01','date-before':'2023-01-01'},False),
            ('date_before_range',{'date-before':'2025-01-01','post-range':'0'},False),
            ('date_once_per_directory',{'date-before':'2025-01-01'},False),
        ]:
            cases.append(run(family,name,selection,row={'name':'Cat','date':'PRIVATE_DATE'},mode=mode,child_pages=[[42,41]] if name=='date_once_per_directory' else None))
        cases.append(run(family,'bad_child_then_valid_child',{'date-before':'2025-01-01'},
            row=[{'name':'Bad','date':'PRIVATE_INVALID_DATE'},{'name':'Good','date':None}],child_pages=[[42],[41]]))
        for name,selection in [
            ('before',{'date-before':'2027-01-01'}),('after',{'date-after':'2025-01-01'}),
            ('filter_before',{'date-after':'2025-01-01','post-filter':'False'}),
            ('range_after',{'date-before':'2027-01-01','post-range':'0'}),
            ('invalid_bound',{'date-before':'PRIVATE_INVALID_BOUND'}),('zero_bound',{'date-after':0})]:
            cases.append(run(family,'popular_'+name,selection,is_popular=True))
    # Append class-scoped options and Python date-bound coercion probes.
    for family in ('danbooru','e621','e926'):
        cases.append(run(family,'root_after_child_before',{'date-after':'2025-01-01'},mode=False,
            tag_selection={'date-before':'2025-01-01'}))
        cases.append(run(family,'root_false_filter_child_true',{'date-after':'2025-01-01','post-filter':'False'},mode=False,
            tag_selection={'date-before':'2025-01-01','post-filter':'True'}))
        for label,bound in [('positive_float',1.5),('negative_float',-1.5),('subsecond_float',0.1),
                            ('boolean_true',True),('string_zero','0'),('invalid_year','10000-01-01')]:
            cases.append(run(family,'bound_'+label,{'date-before':bound},mode=False))
    for family in ('danbooru','e621','e926'):
        cases.append(run(family,'inverted_window_ignores_before',{'date-before':'2022-01-01','date-after':'2023-01-01'},mode=False,child_pages=[[42,41]]))
    return cases


if __name__=='__main__':
    files={'job':Path(inspect.getfile(job)),'util':Path(inspect.getfile(util)),'dt':Path(inspect.getfile(dt)),
           'artist_runner':Path(artists.__file__),'popular_runner':Path(popular.__file__)}
    result={'source':'gallery-dl','version':'1.32.17','frozen_today':popular.TODAY,
            'source_sha256':{name:hashlib.sha256(path.read_bytes()).hexdigest() for name,path in files.items()},
            'notes':['Dan/E621 inherit skip_date=None; no native-style unordered-date flag exists upstream.',
                     'helper_cases artificial skip callbacks exercise real util.predicate_date, not an extractor configuration.',
                     'TypeErrors caught by child Job.run set status1; actual observer records errors even when runner source_errors is empty.'],
            'cases':build(),'helper_cases':helper_cases()}
    Path(__file__).with_name('booru_date_selection.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
