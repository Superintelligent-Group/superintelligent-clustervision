import argparse,pathlib,json,sqlite3,subprocess,os,shutil,hashlib
parser=argparse.ArgumentParser(description='Only one-append listing composition, using the prior synthetic fixture; setup rewrites only the known generated append record')
parser.add_argument('--cv',type=pathlib.Path,required=True)
parser.add_argument('--diagnostic-cv',type=pathlib.Path,required=True)
parser.add_argument('--prepared-control',type=pathlib.Path,required=True,help='prior limited-list-operation control directory with seed and harness-home')
parser.add_argument('--output',type=pathlib.Path,required=True)
a=parser.parse_args();E=a.output.resolve();E.mkdir(parents=True,exist_ok=False);old=a.prepared_control.resolve(strict=True);H=old/'harness-home';source=H/'.claude/projects/-fixed/candidate-063.jsonl';C=E/'control/catalog';C.parent.mkdir();S=E/'control/seed';normal=a.cv.resolve(strict=True);diag=a.diagnostic_cv.resolve(strict=True)

env={'PATH':os.environ['PATH'],'HOME':str(H),'CLUSTERVISION_HOME':str(C),'XDG_CONFIG_HOME':str(E/'config'),'XDG_DATA_HOME':str(E/'data'),'XDG_CACHE_HOME':str(E/'cache'),'LC_ALL':'C','TZ':'UTC','RAYON_NUM_THREADS':'1','CLUSTERVISION_MAX_STALE_SECS':'3600','GIT_CONFIG_GLOBAL':'/dev/null'}
def invoke(binary,args,extras={}):
 r=subprocess.run([str(binary),*args],env=dict(env,**extras),capture_output=True,check=True);assert not r.stderr,r.stderr;return r.stdout
full=source.read_bytes();lines=full.splitlines(keepends=True);assert json.loads(lines[-1])['uuid']=='append';prefix=b''.join(lines[:-1]);append=lines[-1]
source.write_bytes(prefix) # controlled preparation, owned synthetic source only
shutil.copytree(old/'seed',C)
with sqlite3.connect(C/'catalog.db') as db:db.execute('delete from scan_cache');db.commit();db.execute('pragma wal_checkpoint(TRUNCATE)')
setup=invoke(normal,['ls','--json','--fresh','--harness','claude','--limit','100000']);assert len(json.loads(setup))==64
with sqlite3.connect(C/'catalog.db') as db:
 assert db.execute('select count(*) from scan_cache where continuation is not null').fetchone()==(64,)
 state=json.loads(db.execute('select continuation from scan_cache where path=?',(str(source),)).fetchone()[0]);db.execute('pragma wal_checkpoint(TRUNCATE)')
shutil.copytree(C,S)
with source.open('ab') as f:f.write(append)
assert source.read_bytes()==full
(E/'control/setup.json').write_text(json.dumps({'continuation_rows':64,'changed_source':str(source),'prefix_bytes':len(prefix),'append_bytes':len(append),'final_bytes':len(full),'prefix_sha256':hashlib.sha256(prefix).hexdigest(),'final_sha256':hashlib.sha256(full).hexdigest(),'checkpoint':state},indent=2)+'\n')
def identity():
 return [(str(p.relative_to(H)),p.stat().st_dev,p.stat().st_ino,p.stat().st_size,p.stat().st_mtime_ns,p.stat().st_ctime_ns,hashlib.sha256(p.read_bytes()).hexdigest()) for p in sorted(H.rglob('*')) if p.is_file()]
inputs=identity();seedhash=hashlib.sha256((S/'catalog.db').read_bytes()).hexdigest()
args=['ls','--json','--harness','claude','--limit','1','-q','text:needle AND has:plan'];results={}
for name,binary,extra in [('default',normal,{}),('limited',diag,{}),('full-filter-oracle',diag,{'CV_LIST_FULL_ORACLE':'1'})]:
 shutil.rmtree(C);shutil.copytree(S,C);work=E/'control'/f'{name}-work.json';output=invoke(binary,args,dict(extra,**({'CV_LIST_WORK_FILE':str(work)} if name!='default' else {})));(E/'control'/f'{name}.json').write_bytes(output)
 results[name]={'work':json.loads(work.read_text()) if work.exists() else None,'output_sha256':hashlib.sha256(output).hexdigest(),'output_bytes':len(output),'output_rows':len(json.loads(output))}
 assert identity()==inputs and hashlib.sha256((S/'catalog.db').read_bytes()).hexdigest()==seedhash
 with sqlite3.connect(C/'catalog.db') as db:assert json.loads(db.execute('select continuation from scan_cache where path=?',(str(source),)).fetchone()[0])['head']['offset']==len(full)
outputs=[(E/'control'/f'{name}.json').read_bytes() for name in results];assert outputs[0]==outputs[1]==outputs[2]
oldoutput=(old/'runs/one-append-metadata.json').read_bytes();assert outputs[0]==oldoutput
report={'source':'99c8d421','command':args,'fixture_sessions':64,'fixture_reused':str(H),'setup':'new composed warm continuation seed prepared before known append; original seed preserved; all commands use frozen appended source state','prefix_bytes':len(prefix),'append_bytes':len(append),'final_source_bytes':len(full),'seed_catalog_sha256':seedhash,'frozen_source_identity_sha256':hashlib.sha256(json.dumps(inputs).encode()).hexdigest(),'binary_sha256':{str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in [normal,diag]},'results':results,'default_limited_full_filter_and_prior_output_equal':True,'limits':['Metadata navigated bytes count only bytes_at supplied suffix, not all operation reads.','Sampled-prefix guard requested bytes separately count both sampled reads, including overlap; kernel/page I/O and timings unmeasured.','Full-filter oracle uses the existing full predicate reducer before ranking; same seeded counts/index state as default/limited.']}
(E/'summary.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
