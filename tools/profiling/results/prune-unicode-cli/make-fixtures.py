"""Recreate the frozen synthetic 8 MiB controls, without reading harness stores."""
import pathlib,hashlib,json
root=pathlib.Path(__file__).resolve().parent
text=(root/'fixture-template.jsonl').read_text().replace('__GIANT_FIELD__','x'*(8*1024*1024))
for case,payload in [('ascii',text),('emoji',text.replace('ALPHA beta ','🙂 ALPHA beta ',1)),('sigma',text.replace('x'*(8*1024*1024),'x'*(8*1024*1024)+' ΟΣ ',1))]:
 path=root/'fixtures'/case/'source.jsonl';path.parent.mkdir(parents=True,exist_ok=True);path.write_text(payload)
 print(case,len(payload.encode()),hashlib.sha256(payload.encode()).hexdigest())
