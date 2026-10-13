"""Fixed three-case actual CLI prune control; no timings or real harness stores."""
import pathlib,json,os,subprocess,shutil,sqlite3,hashlib,statistics,importlib.util
R=pathlib.Path('/scratch/prune-unicode-cpu-control');work=R/'run';work.mkdir(exist_ok=True);home=work/'home';source=home/'.claude/projects/-fixed/old.jsonl';source.parent.mkdir(parents=True,exist_ok=True);catalog=work/'catalog';binary=work/'cv';seedroot=R/'seeds';seedroot.mkdir(exist_ok=True);tmp=work/'tmp';tmp.mkdir(exist_ok=True)
spec=importlib.util.spec_from_file_location('cg',R/'before/tools/profiling/cachegrind.py');cg=importlib.util.module_from_spec(spec);spec.loader.exec_module(cg)
env={'PATH':'/usr/bin:/bin','HOME':str(home),'CLUSTERVISION_HOME':str(catalog),'XDG_CONFIG_HOME':str(work/'config'),'XDG_DATA_HOME':str(work/'data'),'XDG_CACHE_HOME':str(work/'cache'),'TMPDIR':str(tmp),'LC_ALL':'C','TZ':'UTC','RAYON_NUM_THREADS':'1','CLUSTERVISION_MAX_STALE_SECS':'3600','NO_COLOR':'1','TERM':'dumb'}

reference=json.loads((R/'profiles/sigma/before/summary.json').read_text())
assert cg.fingerprint(source)==reference['input_sha256'] and cg.metadata_fingerprint(source)==reference['input_identity_sha256']
seed=R/'seeds/sigma';assert cg.fingerprint(seed)==reference['seed_sha256']
out=R/'profiles/sigma/fixed';out.mkdir(exist_ok=True)
counts=[]
for label in ['native','1','2']:
 shutil.copyfile(R/'cv-fixed',binary);binary.chmod(0o755)
 for p in source.parent.iterdir():
  if p!=source:assert p.is_file();p.unlink()
 for p in [home,*home.rglob('*')]:
  if p.is_dir():os.utime(p,(1700000000,1700000000))
 shutil.rmtree(catalog);shutil.copytree(seed,catalog)
 command=reference['command']
 argv=command if label=='native' else ['valgrind','--tool=cachegrind','--cache-sim=no','--branch-sim=no','--log-file='+str(out/f'valgrind-{label}.log'),'--cachegrind-out-file='+str(out/f'cachegrind-{label}.out'),*command]
 child=subprocess.run(argv,env=env,cwd=work,capture_output=True,check=True)
 (out/f'stdout-{label}.json').write_bytes(child.stdout);(out/f'stderr-{label}.txt').write_bytes(child.stderr)
 generated={str(p.relative_to(source.parent)):{'bytes':p.stat().st_size,'sha256':cg.fingerprint(p)} for p in sorted(source.parent.iterdir()) if p!=source}
 semantic={'stdout_sha256':hashlib.sha256(child.stdout).hexdigest(),'stderr_sha256':hashlib.sha256(child.stderr).hexdigest(),'result':json.loads(child.stdout),'generated':generated}
 assert semantic==reference['semantic'],semantic
 assert cg.fingerprint(source)==reference['input_sha256'] and cg.metadata_fingerprint(source)==reference['input_identity_sha256'] and cg.fingerprint(seed)==reference['seed_sha256']
 if label!='native':
  lines=(out/f'cachegrind-{label}.out').read_text().splitlines();events=next(x.split()[1:] for x in lines if x.startswith('events:'));totals=next(x.split()[1:] for x in lines if x.startswith('summary:'));counts.append(int(dict(zip(events,totals))['Ir']))
 print(label,counts,flush=True)
result={**reference,'Ir':counts,'binary_sha256':cg.fingerprint(R/'cv-fixed'),'repair_source':'f03ef337','exact_parity':True,'instruction_change_percent':100*(statistics.mean(counts)/statistics.mean(reference['Ir'])-1)}
(out/'summary.json').write_text(json.dumps(result,indent=2,ensure_ascii=False)+'\n')
print(result['instruction_change_percent'],flush=True)
