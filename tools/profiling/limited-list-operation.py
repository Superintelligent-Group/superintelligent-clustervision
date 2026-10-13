import argparse,pathlib,os,json,subprocess,shutil,sqlite3,hashlib,datetime
parser=argparse.ArgumentParser(description="Actual limited-list whole-operation control; synthetic scratch data only")
parser.add_argument('--cv',type=pathlib.Path,required=True)
parser.add_argument('--diagnostic-cv',type=pathlib.Path,required=True)
parser.add_argument('--output',type=pathlib.Path,required=True)
a=parser.parse_args();R=a.output.resolve();R.mkdir(parents=True,exist_ok=False)
D=R/'control';D.mkdir();H=D/'harness-home';P=H/'.claude/projects/-fixed';P.mkdir(parents=True);C=D/'catalog';S=D/'seed';out=D/'runs';out.mkdir()
normal=a.cv.resolve(strict=True);diag=a.diagnostic_cv.resolve(strict=True)
env={'PATH':os.environ['PATH'],'HOME':str(H),'CLUSTERVISION_HOME':str(C),'XDG_CONFIG_HOME':str(D/'config'),'XDG_DATA_HOME':str(D/'data'),'XDG_CACHE_HOME':str(D/'cache'),'LC_ALL':'C','TZ':'UTC','RAYON_NUM_THREADS':'1','CLUSTERVISION_MAX_STALE_SECS':'3600'}
for i in range(64):
 sid=f'candidate-{i:03}';timestamp=(datetime.datetime(2026,1,1,1,4,tzinfo=datetime.timezone.utc)-datetime.timedelta(minutes=i)).isoformat().replace('+00:00','Z')
 blocks=[{'type':'text','text':'response'}]
 if i>=2:blocks.append({'type':'tool_use','id':f'call-{i}','name':'TodoWrite','input':{'todos':[{'content':'step','status':'pending'}]}})
 rows=[{'type':'user','uuid':f'u{i}','sessionId':sid,'timestamp':timestamp,'message':{'role':'user','content':'needle' if i else 'unrelated'}},{'type':'assistant','uuid':f'a{i}','sessionId':sid,'timestamp':timestamp,'message':{'role':'assistant','content':blocks}},{'type':'custom-title','customTitle':f'metadata {i:03}'}]
 (P/f'{sid}.jsonl').write_text(''.join(json.dumps(x)+'\n' for x in rows))
for p in [H,*H.rglob('*')]:os.utime(p,(1700000000,1700000000))
def run(binary,args,extra={}):
 child=subprocess.run([str(binary),*args],env=dict(env,**extra),capture_output=True,check=True);assert not child.stderr or args==['index'],child.stderr;return child.stdout
run(normal,['ls','--json','--fresh','--limit','100000']);run(normal,['index'])
with sqlite3.connect(C/'catalog.db') as db:
 db.execute('delete from affordance_counts');db.execute("delete from source_dependencies where consumer='counts'");db.commit();db.execute('pragma wal_checkpoint(TRUNCATE)')
shutil.copytree(C,S)
def identity():
 return [(str(p.relative_to(H)),p.stat().st_dev,p.stat().st_ino,p.stat().st_size,p.stat().st_mtime_ns,p.stat().st_ctime_ns,hashlib.sha256(p.read_bytes()).hexdigest()) for p in sorted(H.rglob('*')) if p.is_file()]
inputs=identity();seedhash=hashlib.sha256((S/'catalog.db').read_bytes()).hexdigest();results={}
def case(name,args,seed=S,oracle=False,legacy=False):
 shutil.rmtree(C);shutil.copytree(seed,C);expected=run(normal,args)
 shutil.rmtree(C);shutil.copytree(seed,C);work=out/f'{name}-work.json';actual=run(diag,args,{'CV_LIST_WORK_FILE':str(work),**({'CV_LIST_FULL_ORACLE':'1'} if oracle else {}),**({'CV_LIST_ZERO_LEGACY':'1'} if legacy else {})});assert actual==expected,(name,actual,expected)
 (out/f'{name}.json').write_bytes(actual);results[name]={'command':args,'work':json.loads(work.read_text()),'output_sha256':hashlib.sha256(actual).hexdigest(),'output_rows':len(json.loads(actual)),'output_bytes':len(actual),'default_diagnostic_parity':True,'frozen_source_identity_sha256':hashlib.sha256(json.dumps(inputs).encode()).hexdigest()}
 assert identity()==inputs and hashlib.sha256((S/'catalog.db').read_bytes()).hexdigest()==seedhash
 return actual
q='text:needle AND has:plan';args=['ls','--json','--harness','claude','--limit','1','-q',q]
limited=case('limited-cold-counts',args)
with sqlite3.connect(C/'catalog.db') as db:db.execute('pragma wal_checkpoint(TRUNCATE)')
warm=D/'warm-seed';shutil.copytree(C,warm)
case('limited-warm-counts',args,warm)
full=case('full-filter-oracle',args,oracle=True);assert limited==full
case('boolean-metadata-or', ['ls','--json','--harness','claude','--limit','1','-q','title:metadata OR (text:needle AND has:plan)'])
case('boolean-metadata-and-false', ['ls','--json','--harness','claude','--limit','1','-q','title:absent AND has:plan'])
case('boolean-text-or', ['ls','--json','--harness','claude','--limit','1','-q','text:needle OR has:plan'])
case('limit-zero',['ls','--json','--limit','0','--fresh','--enrich','-q',q])
case('limit-zero-legacy',['ls','--json','--limit','0','--fresh','--enrich','-q',q],legacy=True)
case('forced-fresh-metadata',args+['--fresh'])
changed=P/'candidate-063.jsonl'
with changed.open('a') as f:f.write(json.dumps({'type':'user','uuid':'append','sessionId':'candidate-063','timestamp':'2026-01-01T00:01:00Z','message':{'role':'user','content':'appended needle'}})+'\n')
inputs=identity() # controlled mutation occurs outside both command invocations
case('one-append-metadata',args)

report={'source_base':'f08377be','method':'actual cmd_ls default debug CLI versus diagnostic-only same source + counters, same fixed restored seed/canonical inputs, no timings','fixture_sessions':64,'fixture_bytes':sum(p.stat().st_size for p in P.glob('*.jsonl')),'source_identity_and_content_sha256':hashlib.sha256(json.dumps(inputs).encode()).hexdigest(),'seed_catalog_sha256':seedhash,'build_jobs':2,'rayon_threads':1,'binary_sha256':{str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in [normal,diag]},'results':results,'limited_full_filter_bytes_equal':True}
(R/'summary.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
