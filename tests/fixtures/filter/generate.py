import json,sys,unicodedata,random
from pathlib import Path

def contains(values,elements,separator=' '):
    if isinstance(values,str) and (separator or separator is None): values=values.split(separator)
    if not isinstance(elements,(tuple,list)): return elements in values
    return any(e in values for e in elements)
expressions=[
'True','False','None','[]','{}','()','[0]','[0][0]','not []','not [False]',
'1 == True','0 == False','[True] == [1]',"{'x': True} == {'x': 1}",
'(1, 2) == [1, 2]','(1,) == [1]','() == []','(1, 2) == (1, 2)',
'1 < 2 < 3','1 < 2 > 0','3 < 2 < missing','False and missing','True or missing',
"'a' if True else missing","missing if False else 'b'",'not 1 == 2','not 1 in [2]',
'-2**2 == -4','2**3**2 == 512','2**-2 == 0.25','(-2)**2 == 4',
'-7//3 == -3','7//-3 == -3','-7%3 == 2','7%-3 == -2','1.0//0.1 == 9.0',
'1.0%0.1 < 0.1','-1.0%0.1 > 0','-0.0 == 0.0','1/0','0//0','0%0','0**-1',
'9007199254740993 > 9007199254740992.0','9007199254740993 != 9007199254740992.0',
'9007199254740992 == 9007199254740992.0','-9007199254740993 < -9007199254740992.0',
'18446744073709551615 < 18446744073709551616.0','2**120 == 1329227995784915872903807060280344576',
"'cat' in 'a cat'","'cat' not in 'concatenate'","'x' in {'x': 1}","'x' not in {'y': 1}",
"(1, 2)[-1] == 2","[1, 2][-2] == 1","'a🍂b'[1] == '🍂'","len('a🍂b') == 3",
"'a🍂b'[-1] == 'b'","'a'[2]","[1][1]","{'x': 1}['missing']",
"[1] + [2] == [1,2]","(1,) + (2,) == (1,2)","[1] * 3 == [1,1,1]","(1,)*2 == (1,1)",
"'ab'*3 == 'ababab'","3*'ab' == 'ababab'","'ab'*-1 == ''",
"[1] == (1,)","[1] < [2]","(1,2) < (1,3)","[1] < (2,)",
"'CaT'.lower() == 'cat'","'straße'.upper() == 'STRASSE'","'İ'.lower() == 'i̇'",
"' hello '.strip() == 'hello'","'abccab'.strip('ab') == 'cc'",
"'abab'.replace('a','x') == 'xbxb'","'abab'.replace('a','x',1) == 'xbab'",
"'cat'.startswith(('dog','ca'))","'cat'.endswith(('og','at'))","'cat'.startswith('ca')",
"'🍂🍂'.count('🍂') == 2","'🍂a'.find('a') == 1","'abc'.find('z') == -1",
"'a b'.split() == ['a','b']","'a,,b'.split(',') == ['a','','b']","'-'.join(('a','b')) == 'a-b'",
"{'a': 1}.get('a') == 1","{'a': 1}.get('b',2) == 2","{'a':1}.get('b') is None",
"bool(0) == False","str(None) == 'None'","str(True) == 'True'","int('123') == 123","float('1.25') == 1.25",
"abs(-3) == 3","min([3,1,2]) == 1","max(3,1,2) == 3","any([False,0,1])","all([True,1])",
'round(2.5) == 2','round(3.5) == 4','round(-2.5) == -2','round(2.675,2) == 2.67',
"contains('cat dog', ('fox','dog'))","contains('cat dog', 'at')","contains('cat dog', 'at', '')",
"contains('cat,dog', ['dog'], ',')","contains(['cat','dog'], ('fox','cat'))",
]
expressions += [
 "str({'b': 2, 'a': 1}) == \"{'b': 2, 'a': 1}\"",
 "{'b': 2, 'a': 1} == {'a': 1, 'b': 2}",
 "str({'b': 2, 'a': 1, 'b': 3}) == \"{'b': 3, 'a': 1}\"",
 "str(1e20) == '1e+20'", "str(1e-7) == '1e-07'", "str(1e-4) == '0.0001'",
 "str(1e16) == '1e+16'", "str(1e15) == '1000000000000000.0'",
 "round(-6.118090425525051e19,-6) == -6.1180904255251e19",
 "round(6.127673492813808e19,-5) == 6.12767349281381e19",
 "round(3.6370517767371194e19,-15) == 3.6371e19",
]
expressions += [
 '566186495318961628974 / 405290175416300189998 == 1.3969904272596647',
 '(-170141183460469231731687303715884105727 - 1) % -1 == 0',
 'round(1e-310,309) == 0.0', 'round(5e-324,309) == 0.0',
]
rng=random.Random(829170)
for _ in range(64):
    numerator=rng.randrange(-(1<<126),1<<126)
    denominator=rng.randrange(1,1<<126)
    if rng.randrange(2): denominator=-denominator
    expressions.append(f'{numerator} / {denominator} == {numerator / denominator!r}')
for _ in range(64):
    value=rng.uniform(-9,9) * 10.0**rng.randrange(-300,300)
    digits=rng.randrange(-310,324)
    try: rounded=round(value,digits)
    except Exception: continue
    expressions.append(f'round({value!r}, {digits}) == {rounded!r}')
results=[]
for expression in expressions:
    try: result=bool(eval(expression,{'contains':contains}))
    except Exception: result=False
    results.append({'expression':expression,'result':result})
Path('tests/fixtures/filter/python.json').write_text(json.dumps({'python':sys.version.split()[0],'unicode':unicodedata.unidata_version,'cases':results},ensure_ascii=False,indent=2)+'\n')
print(len(results),'reference cases; Python',sys.version.split()[0],'Unicode',unicodedata.unidata_version)
