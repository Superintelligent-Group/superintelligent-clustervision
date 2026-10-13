"""One matched whole actual CLI Claude→Cline Native port; synthetic stores only."""
import pathlib,json,os,subprocess,shutil,sqlite3,hashlib,statistics,importlib.util
R=pathlib.Path('/scratch/cline-port-cpu-control');work=R/'run';home=work/'home';source=home/'.claude/projects/-fixed/old.jsonl';catalog=work/'catalog';binary=work/'cv';output=work/'output';tmp=work/'tmp'
source.parent.mkdir(parents=True,exist_ok=True);tmp.mkdir(exist_ok=True)
spec=importlib.util.spec_from_file_location('cg',R/'before/tools/profiling/cachegrind.py');cg=importlib.util.module_from_spec(spec);spec.loader.exec_module(cg)
env={'PATH':'/usr/bin:/bin','HOME':str(home),'CLUSTERVISION_HOME':str(catalog),'XDG_CONFIG_HOME':str(work/'config'),'XDG_DATA_HOME':str(work/'data'),'XDG_CACHE_HOME':str(work/'cache'),'TMPDIR':str(tmp),'LC_ALL':'C','TZ':'UTC','RAYON_NUM_THREADS':'1','CLUSTERVISION_MAX_STALE_SECS':'3600','NO_COLOR':'1','TERM':'dumb'}
assert cg.fingerprint(R/'cv-before')!=cg.fingerprint(R/'cv-after')
seed=R/'seed'
if not seed.exists():
 shutil.copyfile(R/'source.jsonl',source)
 for p in [home,*home.rglob('*')]:
  if p.is_dir() or p==source:os.utime(p,(1700000000,1700000000))
 shutil.copyfile(R/'cv-before',binary);binary.chmod(0o755)
 assert not catalog.exists(),'Refuse to replace previous preparation without frozen seed.'
 catalog.mkdir()
 setup=subprocess.run([str(binary),'ls','--json','--fresh','--harness','claude','--limit','100000'],env=env,capture_output=True,check=True)
 refs=json.loads(setup.stdout);assert len(refs)==1 and refs[0]['id']=='old',refs
 with sqlite3.connect(catalog/'catalog.db') as db:db.execute('pragma wal_checkpoint(TRUNCATE)')
 shutil.copytree(catalog,seed)
assert cg.fingerprint(source)==cg.fingerprint(R/'source.jsonl')
seed_sha=cg.fingerprint(seed);source_sha=cg.fingerprint(source);source_identity=cg.metadata_fingerprint(source);directories={p:p.stat().st_mtime_ns for p in [home,*home.rglob('*')] if p.is_dir()}
command=[str(binary),'port','claude:old','--harness','cline','--out',str(output),'--no-context','--thinking','native']
def execute(variant,label):
 shutil.copyfile(R/f'cv-{variant}',binary);binary.chmod(0o755);out=R/'profiles'/variant;out.mkdir(parents=True,exist_ok=True)
 if output.exists():shutil.rmtree(output)
 for p,mtime in directories.items():os.utime(p,ns=(mtime,mtime))
 shutil.rmtree(catalog);shutil.copytree(seed,catalog)
 argv=command if label=='native' else ['valgrind','--tool=cachegrind','--cache-sim=no','--branch-sim=no','--log-file='+str(out/f'valgrind-{label}.log'),'--cachegrind-out-file='+str(out/f'cachegrind-{label}.out'),*command]
 child=subprocess.run(argv,env=env,cwd=work,capture_output=True)
 (out/f'stdout-{label}.txt').write_bytes(child.stdout);(out/f'stderr-{label}.txt').write_bytes(child.stderr);child.check_returncode()
 assert 'timestamps: 4 → 1' in child.stderr.decode() and 'message ids: 4 → 0' in child.stderr.decode(),child.stderr
 generated={str(p.relative_to(output)):{'bytes':p.stat().st_size,'sha256':cg.fingerprint(p)} for p in sorted(output.rglob('*')) if p.is_file()}
 assert len(generated)==3 and {pathlib.Path(p).name for p in generated}=={'api_conversation_history.json','task_metadata.json','ui_messages.json'},generated
 history=next(output.rglob('api_conversation_history.json'));rows=json.loads(history.read_bytes());assert len(rows)==4 and [r['role'] for r in rows]==['user','assistant','user','assistant']
 original=json.loads(source.read_text().splitlines()[1])['message']['content'][0]['text'];assert rows[1]['content'][0]['text']==original
 semantic={'stdout_sha256':hashlib.sha256(child.stdout).hexdigest(),'stderr_sha256':hashlib.sha256(child.stderr).hexdigest(),'generated':generated,'roles':[r['role'] for r in rows],'giant_payload_bytes':len(original.encode()),'fidelity_warnings':child.stderr.decode(),'normalization':'none; fixed source timestamp determines task ID'}
 assert cg.fingerprint(source)==source_sha and cg.metadata_fingerprint(source)==source_identity and cg.fingerprint(seed)==seed_sha
 instructions=None
 if label!='native':
  lines=(out/f'cachegrind-{label}.out').read_text().splitlines();events=next(x.split()[1:] for x in lines if x.startswith('events:'));totals=next(x.split()[1:] for x in lines if x.startswith('summary:'));instructions=int(dict(zip(events,totals))['Ir'])
 return semantic,instructions
native={v:execute(v,'native')[0] for v in ['before','after']};assert native['before']==native['after'],native
variants={}
for v in ['before','after']:
 counts=[]
 for label in ['1','2']:
  semantic,n=execute(v,label);assert semantic==native[v];counts.append(n)
 variants[v]={'Ir':counts,'binary_sha256':cg.fingerprint(R/f'cv-{v}'),'semantic':native[v]};print(v,counts,flush=True)
old=statistics.mean(variants['before']['Ir']);new=statistics.mean(variants['after']['Ir'])
report={'baseline':'56e94da5','command':command,'environment':env,'source_sha256':source_sha,'source_identity_sha256':source_identity,'frozen_seed_sha256':seed_sha,'variants':variants,'instruction_change_percent':100*(new/old-1),'exact_parity':True}
(R/'summary.json').write_text(json.dumps(report,indent=2,ensure_ascii=False)+'\n');print(report['instruction_change_percent'],flush=True)
if new>old:
 for v in ['before','after']:
  out=R/'profiles'/v
  with (out/'annotate-1.txt').open('w') as f:subprocess.run(['cg_annotate','--auto=no','--show=Ir','--threshold=0',str(out/'cachegrind-1.out')],stdout=f,check=True)
