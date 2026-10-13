"""Only the settled shared-wire repair on the original frozen whole-port control."""
import pathlib,json,os,subprocess,shutil,hashlib,statistics,importlib.util
R=pathlib.Path('/scratch/cline-port-cpu-control');reference=json.loads((R/'summary.json').read_text());work=R/'run';home=work/'home';source=home/'.claude/projects/-fixed/old.jsonl';catalog=work/'catalog';binary=work/'cv';output=work/'output';seed=R/'seed';env=reference['environment'];command=reference['command']
spec=importlib.util.spec_from_file_location('cg',R/'before/tools/profiling/cachegrind.py');cg=importlib.util.module_from_spec(spec);spec.loader.exec_module(cg)
assert cg.fingerprint(source)==reference['source_sha256'] and cg.metadata_fingerprint(source)==reference['source_identity_sha256'] and cg.fingerprint(seed)==reference['frozen_seed_sha256']
out=R/'profiles/fixed';out.mkdir(exist_ok=True);counts=[]
for label in ['native','1','2']:
 shutil.copyfile(R/'cv-fixed',binary);binary.chmod(0o755)
 if output.exists():shutil.rmtree(output)
 for p in [home,*home.rglob('*')]:
  if p.is_dir():os.utime(p,(1700000000,1700000000))
 shutil.rmtree(catalog);shutil.copytree(seed,catalog)
 argv=command if label=='native' else ['valgrind','--tool=cachegrind','--cache-sim=no','--branch-sim=no','--log-file='+str(out/f'valgrind-{label}.log'),'--cachegrind-out-file='+str(out/f'cachegrind-{label}.out'),*command]
 child=subprocess.run(argv,env=env,cwd=work,capture_output=True)
 (out/f'stdout-{label}.txt').write_bytes(child.stdout);(out/f'stderr-{label}.txt').write_bytes(child.stderr);child.check_returncode()
 generated={str(p.relative_to(output)):{'bytes':p.stat().st_size,'sha256':cg.fingerprint(p)} for p in sorted(output.rglob('*')) if p.is_file()}
 old=reference['variants']['before']['semantic'];assert generated==old['generated'] and hashlib.sha256(child.stdout).hexdigest()==old['stdout_sha256'] and hashlib.sha256(child.stderr).hexdigest()==old['stderr_sha256']
 assert cg.fingerprint(source)==reference['source_sha256'] and cg.metadata_fingerprint(source)==reference['source_identity_sha256'] and cg.fingerprint(seed)==reference['frozen_seed_sha256']
 if label!='native':
  lines=(out/f'cachegrind-{label}.out').read_text().splitlines();events=next(x.split()[1:] for x in lines if x.startswith('events:'));totals=next(x.split()[1:] for x in lines if x.startswith('summary:'));counts.append(int(dict(zip(events,totals))['Ir']))
 print(label,counts,flush=True)
result={'Ir':counts,'binary_sha256':cg.fingerprint(R/'cv-fixed'),'semantic':old,'instruction_change_percent':100*(statistics.mean(counts)/statistics.mean(reference['variants']['before']['Ir'])-1),'source_identity_unchanged':True,'seed_unchanged':True,'exact_parity':True}
(out/'summary.json').write_text(json.dumps(result,indent=2,ensure_ascii=False)+'\n');print(result['instruction_change_percent'],flush=True)
