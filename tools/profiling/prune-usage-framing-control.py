import pathlib,json,subprocess,os,shutil,importlib.util,statistics
R=pathlib.Path('/scratch/prune-cpu-control');work=R/'run';source=work/'input/source.jsonl';binary=work/'prune_probe';directory=R/'profiles/usage/fixed';directory.mkdir()
spec=importlib.util.spec_from_file_location('cg',R/'before/tools/profiling/cachegrind.py');cg=importlib.util.module_from_spec(spec);spec.loader.exec_module(cg)
original=json.loads((R/'profiles/usage/before/summary.json').read_text());candidate=json.loads((R/'profiles/usage/after/summary.json').read_text())
assert cg.fingerprint(source)==original['input_sha256'] and cg.metadata_fingerprint(source)==original['input_identity_sha256']
shutil.copyfile(R/'prune-fixed',binary);binary.chmod(0o755);assert cg.fingerprint(binary)!=candidate['binary_sha256']
env={'PATH':'/usr/bin:/bin','HOME':str(work/'home'),'CLUSTERVISION_HOME':str(work/'catalog'),'TMPDIR':str(work/'tmp'),'LC_ALL':'C','TZ':'UTC','RAYON_NUM_THREADS':'1'};counts=[]
for label in ['native','1','2']:
 for p in source.parent.iterdir():
  if p!=source:assert p.is_file();p.unlink()
 catalog=pathlib.Path(env['CLUSTERVISION_HOME']);shutil.rmtree(catalog);catalog.mkdir()
 command=[str(binary),str(source),'revive'];argv=command if label=='native' else ['valgrind','--tool=cachegrind','--cache-sim=no','--branch-sim=no','--log-file='+str(directory/f'valgrind-{label}.log'),'--cachegrind-out-file='+str(directory/f'cachegrind-{label}.out'),*command]
 child=subprocess.run(argv,env=env,cwd=work,capture_output=True,check=True);assert not child.stderr;(directory/f'stdout-{label}.json').write_bytes(child.stdout);result=json.loads(child.stdout);result.pop('seconds');semantic={'result':result,'generated':{str(p.relative_to(source.parent)):{'bytes':p.stat().st_size,'sha256':cg.fingerprint(p)} for p in sorted(source.parent.iterdir()) if p!=source}};assert semantic==original['semantic']==candidate['semantic']
 assert cg.fingerprint(source)==original['input_sha256'] and cg.metadata_fingerprint(source)==original['input_identity_sha256']
 if label!='native':
  lines=(directory/f'cachegrind-{label}.out').read_text().splitlines();events=next(x.split()[1:] for x in lines if x.startswith('events:'));totals=next(x.split()[1:] for x in lines if x.startswith('summary:'));counts.append(int(dict(zip(events,totals))['Ir']))
report={'Ir':counts,'binary_sha256':cg.fingerprint(binary),'input_sha256':original['input_sha256'],'input_identity_sha256':original['input_identity_sha256'],'semantic':semantic,'native_profiled_before_after_parity':True,'change_percent_vs_before':100*(statistics.mean(counts)/statistics.mean(original['Ir'])-1),'change_percent_vs_candidate':100*(statistics.mean(counts)/statistics.mean(candidate['Ir'])-1)}
(directory/'summary.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
