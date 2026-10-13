"""Two actual prune operations, two variants, two instruction-only repeats; setup excluded."""
import pathlib,subprocess,os,shutil,json,hashlib,importlib.util,statistics
R=pathlib.Path('/scratch/prune-cpu-control');work=R/'run';work.mkdir();inputs=work/'input';inputs.mkdir();binary=work/'prune_probe'
spec=importlib.util.spec_from_file_location('cachegrind',R/'before/tools/profiling/cachegrind.py');cg=importlib.util.module_from_spec(spec);spec.loader.exec_module(cg)
env={'PATH':'/usr/bin:/bin','HOME':str(work/'home'),'CLUSTERVISION_HOME':str(work/'catalog'),'TMPDIR':str(work/'tmp'),'LC_ALL':'C','TZ':'UTC','RAYON_NUM_THREADS':'1'}
for key in ['HOME','TMPDIR']:pathlib.Path(env[key]).mkdir()
assert cg.fingerprint(R/'prune-before')!=cg.fingerprint(R/'prune-after')
report={'controls':{'tool':'valgrind3.24 instruction-only, cache/branch simulation disabled','architecture':os.uname().machine,'rayon_workers':1,'canonical_binary':str(binary),'source_base':'f08377be','candidate':'samef083 tree with only prune.rs8521d265 (combined701d6c09+8521d265)','normalization':'only probe seconds omitted; identical canonical paths need no path normalization','catalog':'fresh empty scratch catalog before each child'},'cases':{}}
for case,modes in [('thinking',['thinking','revive']),('usage',['revive'])]:
 source=inputs/'source.jsonl';shutil.copyfile(R/'fixtures'/case/'source.jsonl',source);identity=cg.metadata_fingerprint(source);sha=cg.fingerprint(source)
 outputs={}
 for variant in ['before','after']:
  shutil.copyfile(R/f'prune-{variant}',binary);binary.chmod(0o755);directory=R/'profiles'/case/variant;directory.mkdir(parents=True);runs=[];semantics=[]
  for label in ['native','1','2']:
   # Remove only this runner's generated artifacts so every operation sees absent destinations.
   for path in inputs.iterdir():
    if path!=source:
     assert path.is_file();path.unlink()
   catalog=pathlib.Path(env['CLUSTERVISION_HOME'])
   if catalog.exists():shutil.rmtree(catalog)
   catalog.mkdir()
   command=[str(binary),str(source),*modes]
   argv=command if label=='native' else ['valgrind','--tool=cachegrind','--cache-sim=no','--branch-sim=no','--log-file='+str(directory/f'valgrind-{label}.log'),'--cachegrind-out-file='+str(directory/f'cachegrind-{label}.out'),*command]
   child=subprocess.run(argv,env=env,cwd=work,capture_output=True,check=True);assert not child.stderr,child.stderr;(directory/f'stdout-{label}.json').write_bytes(child.stdout)
   result=json.loads(child.stdout);result.pop('seconds');assert result['heap'] is None
   generated={str(p.relative_to(inputs)):{'bytes':p.stat().st_size,'sha256':cg.fingerprint(p)} for p in sorted(inputs.iterdir()) if p!=source}
   semantic={'result':result,'generated':generated};semantics.append(semantic)
   assert cg.metadata_fingerprint(source)==identity and cg.fingerprint(source)==sha
   if label!='native':
    lines=(directory/f'cachegrind-{label}.out').read_text().splitlines();events=next(x.split()[1:] for x in lines if x.startswith('events:'));totals=next(x.split()[1:] for x in lines if x.startswith('summary:'));runs.append(int(dict(zip(events,totals))['Ir']))
  assert semantics[0]==semantics[1]==semantics[2]
  outputs[variant]={'Ir':runs,'binary_sha256':cg.fingerprint(binary),'command':command,'input_sha256':sha,'input_identity_sha256':identity,'semantic':semantics[0],'native_profiled_parity':True}
  (directory/'summary.json').write_text(json.dumps(outputs[variant],indent=2)+'\n')
 assert outputs['before']['semantic']==outputs['after']['semantic']
 assert (outputs['after']['semantic']['result']['usage_rewritten']==1) if case=='usage' else (outputs['after']['semantic']['result']['result']['snipped_payloads']==1)
 old=statistics.mean(outputs['before']['Ir']);new=statistics.mean(outputs['after']['Ir']);report['cases'][case]={'variants':outputs,'instruction_change_percent':100*(new/old-1),'before_after_exact_parity':True}
 print(case,outputs['before']['Ir'],outputs['after']['Ir'],100*(new/old-1),flush=True)
 if new>old:
  for variant in ['before','after']:
   directory=R/'profiles'/case/variant
   with (directory/'annotate-1.txt').open('w') as f:subprocess.run(['cg_annotate','--auto=no','--show=Ir','--threshold=0',str(directory/'cachegrind-1.out')],env=env,stdout=f,check=True)
(R/'summary.json').write_text(json.dumps(report,indent=2)+'\n')
