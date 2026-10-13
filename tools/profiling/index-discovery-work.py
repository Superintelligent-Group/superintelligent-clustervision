import argparse, collections, hashlib, json, os, pathlib, sqlite3, subprocess
p=argparse.ArgumentParser(description="Evidence-only native Claude indexing control; requires patched binary and snapshot helper")
p.add_argument("--scratch",required=True,type=pathlib.Path)
p.add_argument("--cv",required=True,type=pathlib.Path)
p.add_argument("--snapshot",required=True,type=pathlib.Path)
a=p.parse_args()
root=a.scratch.resolve()
root.mkdir(exist_ok=False)
home=root/'home';project=root/'project';catalog=root/'catalog';oracle=root/'oracle'
source=home/'.claude/projects/-work/alpha.jsonl'
source.parent.mkdir(parents=True);project.mkdir();catalog.mkdir()
cv=a.cv.resolve()
snapshot=a.snapshot.resolve()
env={'PATH':os.environ['PATH'],'HOME':str(home),'XDG_CACHE_HOME':str(home/'.cache'),'XDG_CONFIG_HOME':str(home/'.config'),'XDG_DATA_HOME':str(home/'.local/share'),'CLUSTERVISION_HOME':str(catalog),'RAYON_NUM_THREADS':'1','CLUSTERVISION_MAX_STALE_SECS':'3600','TZ':'UTC','LC_ALL':'C'}
def canonical(v): return json.dumps(v,sort_keys=True,separators=(',',':'))
def sha(v): return hashlib.sha256(v).hexdigest()
def line(v): return canonical(v)+'\n'
def rec(i,content,role='user',day='01'):
 return dict(type=role,uuid=f'u{i}',sessionId='alpha',timestamp=f'2026-01-{day}T01:00:00Z',cwd=str(project),message=dict(role=role,content=content,model='controlled-model'))
records=[dict(type='custom-title',customTitle='Initial title',timestamp='2026-01-01T00:00:00Z',cwd=str(project)),rec(0,'initial')]
for i in range(64):
 text=('rewriteoldneedle ' if i==16 else f'stablemark{i:02d} ')+('stableword '*14000)
 text=text[:131072]
 records.append(rec(i+1,text,'user' if i%2==0 else 'assistant'))
source.write_text(''.join(map(line,records)))
for d in source.parents:
 if not d.is_relative_to(home): break
 os.utime(d,(1700000000,1700000000))
def command(args,catalog_path=catalog,trace=False):
 e=dict(env,CLUSTERVISION_HOME=str(catalog_path))
 if trace:e['CV_INDEX_WORK_PROBE']='1'
 p=subprocess.run([str(cv),*args],env=e,cwd=root,capture_output=True,check=True)
 return p.stdout,p.stderr
def observe(catalog_path=catalog):
 with sqlite3.connect(catalog_path/'catalog.db') as db:
  meta=db.execute('SELECT harness,id,path,cwd,title,created_at,updated_at,message_count FROM sessions ORDER BY harness,id,path').fetchall()
  counts=[(*row[:3],json.loads(row[3])) for row in db.execute('SELECT harness,id,path,counts FROM affordance_counts ORDER BY harness,id,path')]
  events=db.execute('SELECT s.harness,s.session_id,e.msg_idx,e.ts,e.kind,e.tool,t.target,e.detail FROM events e JOIN event_sessions s ON e.session=s.id LEFT JOIN event_targets t ON e.target_id=t.id ORDER BY 1,2,3,4,5,6,7,8').fetchall()
  mark=db.execute('SELECT offset FROM resume_marks').fetchall()
 p=subprocess.run([str(snapshot),str(catalog_path/'tantivy')],env=env,capture_output=True,check=True)
 docs=sorted(canonical(json.loads(d)) for d in json.loads(p.stdout))
 queries={}
 for q in ['stablemark00','appendneedle','rewriteoldneedle','rewritenewneedle','title:initial','title:appended','title:initial AND body:stablemark00','title:appended AND body:stablemark00']:
  out,_=command(['search',q,'--json'],catalog_path)
  queries[q]=json.loads(out)
 return dict(metadata=meta,counts=counts,events=events,mark=mark,docs=docs,queries=queries)
def index(label,args=(),catalog_path=catalog):
 out,err=command(['index',*args],catalog_path,True)
 (root/(label+'.stdout')).write_bytes(out);(root/(label+'.stderr')).write_bytes(err)
 phase=None;metrics=collections.defaultdict(lambda:collections.defaultdict(lambda:dict(calls=0,bytes=0,offsets=[])))
 for line in err.decode().splitlines():
  if line.startswith('CV_INDEX_PHASE '):phase=line.split(' ',1)[1]
  if line.startswith('CV_INDEX_WORK '):
   x=json.loads(line.split(' ',1)[1]);m=metrics[phase][x['stage']];m['calls']+=1;m['bytes']+=x['bytes']
   if x['offset'] is not None:m['offsets'].append(x['offset'])
 assert 'discovery' in metrics or 'index' in metrics or label=='unchanged'
 work={p:{s:dict(calls=m['calls'],bytes=m['bytes'],first_offset=min(m['offsets']) if m['offsets'] else None,last_offset=max(m['offsets']) if m['offsets'] else None) for s,m in stages.items()} for p,stages in metrics.items()}
 observation=observe(catalog_path)
 parity={k:sha(canonical(v).encode()) for k,v in observation.items() if k!='mark'}
 report=dict(source_bytes=source.stat().st_size,source_sha256=sha(source.read_bytes()),work=work,parity_sha256=parity,metadata=observation['metadata'],counts=observation['counts'],events=observation['events'],mark=observation['mark'],documents=len(observation['docs']),progress=[l for l in err.decode().splitlines() if not l.startswith('CV_INDEX_')],stdout=out.decode())
 (root/(label+'.json')).write_text(json.dumps(report,indent=2)+'\n')
 return observation,report
cold,cold_report=index('cold')
unchanged,unchanged_report=index('unchanged')
assert cold==unchanged
assert not unchanged_report['work']
old_size=source.stat().st_size
with source.open('a') as file:
 file.write(line(dict(type='custom-title',customTitle='Appended title',timestamp='2026-01-02T00:00:00Z')))
 file.write(line(rec(65,'appendneedle user request',day='02')))
 file.write(line(rec(66,[dict(type='text',text='appendneedle assistant response'),dict(type='tool_use',id='edit-1',name='Edit',input=dict(file_path=str(project/'tracked.rs'),old_string='baseline',new_string='rewritten'))],'assistant','02')))
appended,append_report=index('append')
oracle.mkdir()
full,full_report=index('append-full',catalog_path=oracle)
append_parity={k:appended[k]==full[k] for k in ['metadata','counts','events','docs','queries']}
assert all(append_parity[k] for k in ['metadata','counts','events'])
assert appended['metadata'][0][-1]==67
# Same-size middle rewrite, restoring mtime: sampled mark bytes stay outside this edit.
stat=source.stat();before=source.read_bytes();at=before.index(b'rewriteoldneedle')
with source.open('r+b') as f:f.seek(at);f.write(b'rewritenewneedle')
os.utime(source,ns=(stat.st_atime_ns,stat.st_mtime_ns))
assert source.stat().st_size==stat.st_size and source.stat().st_mtime_ns==stat.st_mtime_ns
rewritten,rewrite_report=index('same-size-rewrite')
full_rewrite,full_rewrite_report=index('rewrite-full',['--rebuild'],oracle)
rewrite_parity={k:rewritten[k]==full_rewrite[k] for k in ['metadata','counts','events','docs','queries']}
assert all(rewrite_parity[k] for k in ['metadata','counts','events'])
summary=dict(append_parity=append_parity,rewrite_parity=rewrite_parity,base='c8d16a92',synthetic=True,initial_bytes=old_size,appended_bytes=append_report['source_bytes']-old_size,rewrite_offset=at,operations={k:v for k,v in [('cold',cold_report),('unchanged',unchanged_report),('append',append_report),('append-full',full_report),('same-size-rewrite',rewrite_report),('rewrite-full',full_rewrite_report)]},limits=['Boundary extents and reader returns are separate; not physical I/O.','Native mmap, one synthetic primary, fixed 4 MiB production checkpoint size.','Complete stored documents and controlled indexed queries compared; baseline defects are recorded, not accepted as parity.'])
(root/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
for name,case in summary['operations'].items():
 print(name,case['source_bytes'],case['documents'],case['mark'],canonical(case['work']),case['progress'])
