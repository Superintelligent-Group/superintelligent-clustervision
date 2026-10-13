"""One synthetic mixed ordinary/escaped 8 MiB Claude Native port workload."""
import pathlib,json,hashlib
root=pathlib.Path(__file__).resolve().parent
half=4*1024*1024
pattern='escaped\nλ "\\\t '
escaped=(pattern*((half//len(pattern.encode()))+1)).encode()[:half].decode('utf8',errors='ignore')
escaped+='x'*(half-len(escaped.encode()))
text='x'*half+escaped
assert len(text.encode())==8*1024*1024
rows=[
 {'type':'user','uuid':'u','sessionId':'old','timestamp':'2026-10-12T12:00:00Z','cwd':'/synthetic/export','message':{'role':'user','content':'prompt'}},
 {'type':'assistant','uuid':'a','sessionId':'old','parentUuid':'u','timestamp':'2026-10-12T12:00:00Z','cwd':'/synthetic/export','message':{'role':'assistant','content':[{'type':'text','text':text},{'type':'text','text':'conclusion'}]}},
 {'type':'user','uuid':'v','sessionId':'old','timestamp':'2026-10-12T12:00:00Z','cwd':'/synthetic/export','message':{'role':'user','content':'recent'}},
 {'type':'assistant','uuid':'z','sessionId':'old','timestamp':'2026-10-12T12:00:00Z','cwd':'/synthetic/export','message':{'role':'assistant','content':[{'type':'text','text':'recent answer'}]}},
]
raw=(''.join(json.dumps(x,ensure_ascii=False)+'\n' for x in rows)).encode()
(root/'source.jsonl').write_bytes(raw)
(root/'fixture.json').write_text(json.dumps({'decoded_payload_bytes':len(text.encode()),'ordinary_bytes':half,'escaped_bytes':half,'source_bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),'shape':'four fixed-timestamp Claude messages; one giant Text field; no real-store input'},indent=2)+'\n')
