"""Fixed three-case actual CLI prune control; no timings or real harness stores."""
import pathlib,json,os,subprocess,shutil,sqlite3,hashlib,statistics,importlib.util
R=pathlib.Path('/scratch/prune-unicode-cpu-control');work=R/'run';work.mkdir(exist_ok=True);home=work/'home';source=home/'.claude/projects/-fixed/old.jsonl';source.parent.mkdir(parents=True,exist_ok=True);catalog=work/'catalog';binary=work/'cv';seedroot=R/'seeds';seedroot.mkdir(exist_ok=True);tmp=work/'tmp';tmp.mkdir(exist_ok=True)
spec=importlib.util.spec_from_file_location('cg',R/'before/tools/profiling/cachegrind.py');cg=importlib.util.module_from_spec(spec);spec.loader.exec_module(cg)
env={'PATH':'/usr/bin:/bin','HOME':str(home),'CLUSTERVISION_HOME':str(catalog),'XDG_CONFIG_HOME':str(work/'config'),'XDG_DATA_HOME':str(work/'data'),'XDG_CACHE_HOME':str(work/'cache'),'TMPDIR':str(tmp),'LC_ALL':'C','TZ':'UTC','RAYON_NUM_THREADS':'1','CLUSTERVISION_MAX_STALE_SECS':'3600','NO_COLOR':'1','TERM':'dumb'}
assert cg.fingerprint(R/'cv-before')!=cg.fingerprint(R/'cv-after')
report={'controls':{'operation':'actual cv prune; same CLI gateway all three cases','canonical_binary':str(binary),'resource_cap':'3CPUs8GiB','build_jobs':1,'rayon_workers':1,'source_baseline':'5e366e96','normalization':'none:stdout/stderr bytes and generated files exact;fixedtoID','profile':'optimized defaultfeatures/noheap, instruction-onlyCachegrind','setup':'source/catalog preparation and generated-artifact removal outside countedchild'},'cases':{}}
for case,tokens in [('ascii','alpha,beta'),('emoji','alpha,beta'),('sigma','alpha,ος')]:
 # Completed cases are retained when a later setup assertion blocks the runner.
 if (R/'profiles'/case/'after/summary.json').exists():
  variants={v:json.loads((R/'profiles'/case/v/'summary.json').read_text()) for v in ['before','after']};assert variants['before']['semantic']==variants['after']['semantic'];old=statistics.mean(variants['before']['Ir']);new=statistics.mean(variants['after']['Ir']);report['cases'][case]={'variants':variants,'instruction_change_percent':100*(new/old-1),'exact_parity':True};continue
 # Clear only this runner's previous generated siblings before native inventory setup too.
 for p in source.parent.iterdir():
  if p!=source:assert p.is_file();p.unlink()
 shutil.copyfile(R/'fixtures'/case/'source.jsonl',source)
 for p in [home,*home.rglob('*')]:
  if p.is_dir() or p==source:os.utime(p,(1700000000,1700000000))
 shutil.copyfile(R/'cv-before',binary);binary.chmod(0o755)
 if catalog.exists():shutil.rmtree(catalog)
 catalog.mkdir()
 setup=subprocess.run([str(binary),'ls','--json','--fresh','--harness','claude','--limit','100000'],env=env,capture_output=True,check=True);refs=json.loads(setup.stdout);assert len(refs)==1 and refs[0]['id']=='old',refs
 with sqlite3.connect(catalog/'catalog.db') as db:db.execute('pragma wal_checkpoint(TRUNCATE)')
 seed=seedroot/case;shutil.copytree(catalog,seed);seed_sha=cg.fingerprint(seed);source_sha=cg.fingerprint(source);source_identity=cg.metadata_fingerprint(source);directories={p:p.stat().st_mtime_ns for p in [home,*home.rglob('*')] if p.is_dir()}
 command=[str(binary),'prune','old','--harness','claude','--keep-last','2','--no-revive','--to','22222222-2222-4222-8222-222222222222','--declassify','--declassify-tokens',tokens,'--json'];variants={}
 def execute(variant,label):
  shutil.copyfile(R/f'cv-{variant}',binary);binary.chmod(0o755);out=R/'profiles'/case/variant;out.mkdir(parents=True,exist_ok=True)
  for p in source.parent.iterdir():
   if p!=source:assert p.is_file();p.unlink()
  for p,mtime in directories.items():os.utime(p,ns=(mtime,mtime))
  shutil.rmtree(catalog);shutil.copytree(seed,catalog)
  argv=command if label=='native' else ['valgrind','--tool=cachegrind','--cache-sim=no','--branch-sim=no','--log-file='+str(out/f'valgrind-{label}.log'),'--cachegrind-out-file='+str(out/f'cachegrind-{label}.out'),*command]
  child=subprocess.run(argv,env=env,cwd=work,capture_output=True,check=True);(out/f'stdout-{label}.json').write_bytes(child.stdout);(out/f'stderr-{label}.txt').write_bytes(child.stderr);result=json.loads(child.stdout);assert result['snipped_payloads']==1 and result['new_id']=='22222222-2222-4222-8222-222222222222',result
  generated={str(p.relative_to(source.parent)):{'bytes':p.stat().st_size,'sha256':cg.fingerprint(p)} for p in sorted(source.parent.iterdir()) if p!=source};assert len(generated)==2,generated
  semantic={'stdout_sha256':hashlib.sha256(child.stdout).hexdigest(),'stderr_sha256':hashlib.sha256(child.stderr).hexdigest(),'result':result,'generated':generated}
  assert cg.fingerprint(source)==source_sha and cg.metadata_fingerprint(source)==source_identity and cg.fingerprint(seed)==seed_sha
  instructions=None
  if label!='native':
   lines=(out/f'cachegrind-{label}.out').read_text().splitlines();events=next(x.split()[1:] for x in lines if x.startswith('events:'));totals=next(x.split()[1:] for x in lines if x.startswith('summary:'));instructions=int(dict(zip(events,totals))['Ir'])
  return semantic,instructions
 # Check both actual gateways/fidelity before instrumenting either variant.
 native={variant:execute(variant,'native')[0] for variant in ['before','after']};assert native['before']==native['after']
 for variant in ['before','after']:
  counts=[]
  for label in ['1','2']:
   semantic,instructions=execute(variant,label);assert semantic==native[variant];counts.append(instructions)
  variants[variant]={'Ir':counts,'binary_sha256':cg.fingerprint(R/f'cv-{variant}'),'command':command,'input_sha256':source_sha,'input_identity_sha256':source_identity,'seed_sha256':seed_sha,'semantic':native[variant],'native_profiled_parity':True};(R/'profiles'/case/variant/'summary.json').write_text(json.dumps(variants[variant],indent=2,ensure_ascii=False)+'\n')
 assert variants['before']['semantic']==variants['after']['semantic'];old=statistics.mean(variants['before']['Ir']);new=statistics.mean(variants['after']['Ir']);report['cases'][case]={'variants':variants,'instruction_change_percent':100*(new/old-1),'exact_parity':True}
 print(case,variants['before']['Ir'],variants['after']['Ir'],100*(new/old-1),flush=True)
 if new>old:
  for variant in ['before','after']:
   out=R/'profiles'/case/variant
   with (out/'annotate-1.txt').open('w') as f:subprocess.run(['cg_annotate','--auto=no','--show=Ir','--threshold=0',str(out/'cachegrind-1.out')],stdout=f,check=True)
(R/'summary.json').write_text(json.dumps(report,indent=2,ensure_ascii=False)+'\n')
