#!/usr/bin/env python3
"""Record real gallery-dl extractor class archive formats; no extraction/network."""
import hashlib
import inspect
import json
from pathlib import Path
import gallery_dl
from gallery_dl import extractor, formatter
import copy
import requests

URLS = '''
https://x.com/example
https://x.com/example/media
https://x.com/example/photo
https://x.com/example/header_photo
https://x.com/example/status/123456789
https://x.com/search?q=cat
https://x.com/i/bookmarks
https://x.com/i/lists/123456789
https://x.com/i/communities/123456789
https://x.com/i/communities
https://www.pixiv.net/users/123
https://www.pixiv.net/users/123/artworks
https://www.pixiv.net/users/123/illustrations
https://www.pixiv.net/users/123/manga
https://www.pixiv.net/users/123/artworks/cat
https://www.pixiv.net/users/123/bookmarks/artworks
https://www.pixiv.net/users/123/bookmarks/artworks/cat?rest=hide
https://www.pixiv.net/bookmark.php?id=123
https://www.pixiv.net/bookmark.php?type=user
https://www.pixiv.net/member_illust.php?id=123
https://www.pixiv.net/artworks/123
https://www.pixiv.net/users/123/avatar
https://www.pixiv.net/users/123/background
https://www.pixiv.net/users/123/following
https://www.pixiv.net/ranking.php
https://www.pixiv.net/tags/cat/artworks
https://www.pixiv.net/search.php?word=cat
https://www.pixiv.net/bookmark_new_illust.php
https://www.pixiv.net/user/123/series/456
https://www.pixivision.net/en/a/123
https://sketch.pixiv.net/@example
https://www.pixiv.net/novel/show.php?id=123
https://www.pixiv.net/n/123
https://www.pixiv.net/users/123/novels
https://www.pixiv.net/novel/series/123
https://www.pixiv.net/users/123/bookmarks/novels
https://bsky.app/profile/example.com
https://bsky.app/profile/example.com/post/3k2k3k2k3k2k3
https://bsky.app/profile/example.com/feed/example
https://bsky.app/profile/example.com/likes
https://bsky.app/profile/example.com/avatar
https://www.reddit.com/r/pics/comments/abc123/title/
https://www.reddit.com/r/pics/
https://www.reddit.com/user/example/
https://redd.it/abc123
https://mastodon.social/@example
https://mastodon.social/@example/123456789
https://mastodon.social/bookmarks
https://mastodon.social/favourites
https://mastodon.social/tags/cat
https://example.tumblr.com/
https://example.tumblr.com/post/123456789/title
https://example.tumblr.com/tagged/cat
https://kemono.cr/artists
https://kemono.cr/posts
https://kemono.cr/favorites
https://kemono.cr/patreon/user/123
https://kemono.cr/patreon/user/123/post/456
https://kemono.cr/patreon/user/123/post/456/revisions
https://kemono.cr/discord/server/123
https://kemono.cr/discord/server/123/channel/456
https://coomer.st/onlyfans/user/example
https://imgur.com/abcde
https://imgur.com/a/abcde
https://imgur.com/gallery/abcde
https://www.deviantart.com/example
https://www.deviantart.com/example/gallery
https://www.deviantart.com/example/gallery/12345678/test
https://www.deviantart.com/example/art/test-123456789
https://www.flickr.com/photos/example/
https://www.flickr.com/photos/example/123456789/
https://www.flickr.com/photos/example/albums/123456789/
https://flic.kr/p/2abcde
https://wallhaven.cc/w/abc123
https://wallhaven.cc/search?q=cat
https://wallhaven.cc/user/example/uploads
https://wallhaven.cc/user/example/favorites/123
https://www.artstation.com/example
https://www.artstation.com/artwork/abc123
https://www.pinterest.com/example/pins/
https://www.pinterest.com/example/board/
https://www.pinterest.com/pin/123456789/
https://pin.it/abc123
https://www.newgrounds.com/art/view/example/test
https://example.newgrounds.com/art
https://www.furaffinity.net/view/123456789/
https://www.furaffinity.net/gallery/example/
https://www.furaffinity.net/scraps/example/
https://www.furaffinity.net/favorites/example/
https://www.weasyl.com/~example/submissions/1234567/test
https://www.weasyl.com/submission/1234567/test
https://www.weasyl.com/~example
https://vsco.co/example/gallery
https://vsco.co/example/media/1234567890abcdef12345678
https://pixeldrain.com/u/abcdefgh
https://pixeldrain.com/l/abcdefgh
https://catbox.moe/c/abcdef
https://imgbox.com/abcdefgh
https://imgbox.com/g/abcdefghij
https://postimg.cc/abcdefgh
https://postimg.cc/gallery/abcdefgh
https://www.instagram.com/p/AbCdEfGhIjK/
https://www.instagram.com/example/
https://www.facebook.com/photo.php?fbid=123456789
https://www.facebook.com/watch/?v=123456789
https://danbooru.donmai.us/posts/123
https://danbooru.donmai.us/posts?tags=cat
https://danbooru.donmai.us/pools/123
https://e621.net/posts/123
https://e621.net/posts?tags=cat
https://e621.net/pools/123
https://e926.net/posts/123
https://www.e926.net/posts/123
https://www.e621.net/posts/123
https://www.danbooru.donmai.us/posts/123
https://donmai.us/posts/123
https://www.safebooru.org/index.php?page=post&s=view&id=123
https://www.gelbooru.com/index.php?page=post&s=view&id=123
https://pawoo.net/@example
https://baraag.net/@example
https://gelbooru.com/index.php?page=post&s=view&id=123
https://gelbooru.com/index.php?page=post&s=list&tags=cat
https://safebooru.org/index.php?page=post&s=view&id=123
https://safebooru.org/index.php?page=post&s=list&tags=cat
'''.strip().splitlines()

def key_cases():
    cases = []
    pixiv_input = {
        "id":42,"type":"illust","user":{"id":7,"account":"artist"},
        "title":"Title","caption":"","create_date":"2024-01-02T03:04:05+00:00",
        "tags":[],"x_restrict":0,"image_urls":{},"width":640,"height":480,
        "meta_single_page":{"original_image_url":"https://i.pximg.net/img-original/img/2024/01/02/03/04/05/42_p0.jpg"},
        "meta_pages":[],"is_bookmarked":False,
    }
    for name,pages in [("pixiv_single",0),("pixiv_pages",2)]:
        work=copy.deepcopy(pixiv_input)
        if pages:
            work["meta_pages"]=[{"image_urls":{"original":pixiv_input["meta_single_page"]["original_image_url"].replace("_p0",f"_p{n}")}} for n in range(pages)]
        obj=extractor.find("https://www.pixiv.net/artworks/42")
        obj.session=requests.Session()
        obj._init()
        obj.metadata=lambda:{}
        obj.works=lambda:iter([copy.deepcopy(work)])
        keys=[]
        snapshots=[]
        for msg in obj.items():
            if msg[0]==3:
                keys.append(formatter.parse(obj.archive_fmt).format_map(msg[2]))
                snapshots.append({key:msg[2][key] for key in ("id","num","suffix","extension")})
        cases.append({"name":name,"url":obj.url,"input":work,"keys":keys,"source_keywords":snapshots})
    project={"hash_id":"abc123","id":90,"title":"Project","description":"", "created_at":"2024-01-02T03:04:05+00:00", "published_at":"2024-01-02T03:04:05+00:00", "user":{"username":"artist"},"assets":[{"id":123,"asset_type":"image","has_image":True,"has_embedded_player":False,"image_url":"https://cdna.artstation.com/p/assets/images/images/123/large/example.jpg"}]}
    obj=extractor.find("https://www.artstation.com/artwork/abc123")
    obj.session=requests.Session()
    obj._init()
    obj.metadata=lambda:{}
    obj.projects=lambda:iter([{"hash_id":"abc123"}])
    from gallery_dl.extractor.artstation import ArtstationExtractor
    obj.get_project_assets=lambda ident:ArtstationExtractor.get_project_assets(obj,ident)
    obj.request_json=lambda *a,**kw:copy.deepcopy(project)
    keys=[]
    for msg in obj.items():
        if msg[0]==3: keys.append(formatter.parse(obj.archive_fmt).format_map(msg[2]))
    cases.append({"name":"artstation_asset","url":obj.url,"input":project,"keys":keys})
    return cases

def main():
    cases=[]
    sources={}
    for url in URLS:
        instance=extractor.find(url)
        case={'url':url}
        if '/favorites/example/' in url and 'furaffinity' in url:
            case['native_supported'] = False
            case['native_reason'] = 'Native Fur Affinity currently supports submissions, gallery and scraps only.'
        if instance is None:
            case['reason']='Installed upstream does not match this native route alias.'
            case['descriptor']=None
        else:
            cls=type(instance)
            file=Path(inspect.getfile(cls))
            sources[file.name]=hashlib.sha256(file.read_bytes()).hexdigest()
            case.update(extractor=cls.__name__, subcategory=instance.subcategory,
                        descriptor={'category':instance.category,'format':instance.archive_fmt})
        cases.append(case)
    common=Path(inspect.getfile(extractor.common.Extractor))
    sources[common.name]=hashlib.sha256(common.read_bytes()).hexdigest()
    file=Path(inspect.getfile(formatter))
    sources[file.name]=hashlib.sha256(file.read_bytes()).hexdigest()
    output={'version':gallery_dl.version.__version__,'source_sha256':sources,'cases':cases,'key_cases':key_cases()}
    Path(__file__).with_name('descriptors.json').write_text(json.dumps(output,ensure_ascii=False,indent=2)+'\n')
if __name__=='__main__': main()
