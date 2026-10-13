import os,json,sqlite3,shutil,subprocess,pathlib,hashlib
R=pathlib.Path('/scratch/keyed-discovery-cpu'); assert hashlib.sha256((R/'cv-before').read_bytes()).digest()!=hashlib.sha256((R/'cv-after').read_bytes()).digest(); B=R/'runtime/cv'; B.parent.mkdir(exist_ok=True)
runner=R/'after/tools/profiling/cachegrind.py'; compare=R/'after/tools/profiling/compare.py'
env=dict(os.environ,LC_ALL='C',TZ='UTC',RAYON_NUM_THREADS='1',CLUSTERVISION_MAX_STALE_SECS='3600')
reports={}
for case,n in [('changed',1),('warm',4096)]:
 root=R/case; root.mkdir(); home=root/'home'; directory=home/'.claude/projects/-fixed'; directory.mkdir(parents=True); cat=root/'catalog';cat.mkdir()
 def rec(name,i):return json.dumps({'type':'user','uuid':f'u{i}','sessionId':name,'timestamp':'2026-01-01T00:00:00Z','message':{'role':'user','content':f'hello {i}'}})+'\n'
 for i in range(n):
  name='alpha' if i==0 else f'session-{i:05}'; (directory/f'{name}.jsonl').write_text(rec(name,1)+(json.dumps({'type':'custom-title','customTitle':'metadata '+'x'*1024})+'\n' if case=='warm' else ''))
 for p in [home,*home.rglob('*')]:
  if p.is_dir():os.utime(p,(1700000000,1700000000))
 local=dict(env,HOME=str(home),CLUSTERVISION_HOME=str(cat),XDG_CONFIG_HOME=str(root/'config'),XDG_DATA_HOME=str(root/'data'),XDG_CACHE_HOME=str(root/'cache'))
 command=[str(B),'ls','--json','--harness','claude','--limit','100000']
 shutil.copyfile(R/'cv-before',B);B.chmod(0o755)
 out=subprocess.run(command,env=local,capture_output=True,check=True);assert not out.stderr
 with sqlite3.connect(cat/'catalog.db') as db:
  assert db.execute('select count(*) from scan_cache').fetchone()==(n,)
  if case=='changed':
   original=db.execute('select mtime_ns,size,refs,source_reads,interpretation_version from scan_cache limit 1').fetchone()
   source=directory/'alpha.jsonl'
   for i in range(4096):
    fake=str(root/'unrelated'/f'{i}.jsonl');refs=json.loads(original[2]);refs[0].update(path=fake,id=f'unrelated-{i}',harness='codex',title='metadata '+'x'*1024)
    db.execute('insert into scan_cache(path,mtime_ns,size,refs,source_reads,interpretation_version) values(?,?,?,?,?,?)',(fake,original[0],original[1],json.dumps(refs,separators=(',',':')),original[3].replace(str(source),fake),original[4]))
   db.commit()
  db.execute('pragma wal_checkpoint(TRUNCATE)')
 if case=='changed':
  with (directory/'alpha.jsonl').open('a') as f:f.write(rec('alpha',2))
 else:command.append('--fresh')
 seed=root/'seed';shutil.copytree(cat,seed);shutil.rmtree(cat)
 for label in ['before','after']:
  shutil.copyfile(R/f'cv-{label}',B);B.chmod(0o755)
  args=['python3',str(runner),'--input',str(home),'--harness-home',str(home),'--seed-catalog',str(seed),'--catalog-home',str(cat),'--output','bytes',str(root/label),'--',*command]
  with (root/f'{label}-run.log').open('w') as f:subprocess.run(args,env=env,stdout=f,stderr=subprocess.STDOUT,check=True)
  shutil.rmtree(cat)
  summary=root/label/'summary.json'; data=json.loads(summary.read_text()); data['build']=json.loads((R/'build.json').read_text()); summary.write_text(json.dumps(data,indent=2)+'\n')
 with (root/'comparison.json').open('w') as f:subprocess.run(['python3',str(compare),str(root/'before/summary.json'),str(root/'after/summary.json')],stdout=f,check=True)
 reports[case]=json.loads((root/'comparison.json').read_text())
(R/'comparison.json').write_text(json.dumps(reports,indent=2)+'\n');print(json.dumps(reports,indent=2))
