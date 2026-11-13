// Independent mTLS protocol checks. These are controlled boundary fixtures,
// never evidence of real-model planning (ai_acceptance.py covers that).
import grpc from "@grpc/grpc-js";
import loader from "@grpc/proto-loader";
import {readFileSync, writeFileSync, mkdirSync} from "node:fs";
import {resolve} from "node:path";
import {execFileSync,spawn} from "node:child_process";
import assert from "node:assert/strict";
import {setTimeout as delay} from "node:timers/promises";

process.umask(0o077);
const run=resolve(process.argv[2]);
const access=JSON.parse(readFileSync(resolve(run,"access.json")));
const environment=JSON.parse(readFileSync(resolve(run,"env.json")));
const sourceWorker=JSON.parse(readFileSync(resolve(run,"worker.json")));
const evidence={kind:"controlled-planning-protocol-boundaries",checks:[]};
const check=(name,condition)=>{assert.ok(condition,name);evidence.checks.push(name);console.log("PASS",name);};
const cookie=Object.entries(access.cookies).map(([k,v])=>`${k}=${v}`).join("; ");
async function api(path,body,method){
  const response=await fetch(access.base+"/api/v1"+path,{method:method||(body===undefined?"GET":"POST"),headers:{cookie,"content-type":"application/json",origin:access.base,"x-artifactory-csrf":"1"},body:body===undefined?undefined:JSON.stringify(body)});
  const value=await response.json();assert.equal(response.status,200,`${path}: ${JSON.stringify(value)}`);return value;
}
const database=new URL(environment.DATABASE_URL);
const pg={...process.env,PGHOST:database.hostname,PGPORT:database.port||"5432",PGUSER:decodeURIComponent(database.username),PGPASSWORD:decodeURIComponent(database.password),PGDATABASE:database.pathname.slice(1),PGOPTIONS:database.searchParams.get("options")||""};
function sql(query){return execFileSync("psql",["-X","-qAt","-v","ON_ERROR_STOP=1"],{input:query,env:pg,encoding:"utf8",stdio:["pipe","pipe","pipe"]}).trim();}
async function lockBuild(build){
  const process=spawn("psql",["-X","-qAt","-v","ON_ERROR_STOP=1"],{env:pg,stdio:["pipe","pipe","pipe"]});
  const backend=await new Promise((accept,reject)=>{
    let output="";process.stdout.on("data",chunk=>{output+=chunk;if(output.includes("locked\n"))accept(Number(output.trim().split("\n")[0]));});
    process.once("error",reject);process.once("exit",code=>{if(code)reject(new Error("lock holder failed"));});
    process.stdin.write(`BEGIN;SELECT pg_backend_pid();SELECT id FROM builds WHERE id='${build}' FOR UPDATE;SELECT 'locked';\n`);
  });
  return {backend,release:()=>new Promise(accept=>{process.once("exit",accept);process.stdin.end("COMMIT;\n");})};
}
async function blocked(backend,count){
  for(let i=0;i<200;i++){
    // PostgreSQL queues a second tuple waiter behind the first waiter, so it
    // may be blocked transitively rather than directly by our lock holder.
    if(Number(sql(`WITH RECURSIVE waiters(pid) AS (SELECT pid FROM pg_stat_activity WHERE ${backend}=ANY(pg_blocking_pids(pid)) UNION SELECT a.pid FROM pg_stat_activity a JOIN waiters w ON w.pid=ANY(pg_blocking_pids(a.pid))) SELECT count(*) FROM waiters`))>=count)return;
    await delay(10);
  }
  throw new Error("expected real database lock wait");
}
const definitions=grpc.loadPackageDefinition(loader.loadSync(resolve(import.meta.dirname,"../../../proto/artifactory/emissary/v1/emissary.proto"),{keepCase:true,defaults:true,longs:Number})).artifactory.emissary.v1;
const directory=resolve(run,"protocol-worker-"+Date.now());mkdirSync(directory,{mode:0o700});
const config={...sourceWorker,worker_id:undefined,certificate_file:resolve(directory,"worker.pem"),key_file:resolve(directory,"worker.key"),name:"planning-boundary-probe"};
const path=resolve(directory,"worker.json");writeFileSync(path,JSON.stringify(config),{mode:0o600});
const workerStates=await api("/ai-workers"),nodeStates=await api("/nodes");
const platform=nodeStates.find(n=>n.online)||nodeStates[0];
let client,probeWorker;
const builds=[];
const call=(method,payload)=>new Promise((accept,reject)=>client[method](payload,{deadline:Date.now()+30000},(error,value)=>error?reject(error):accept(value)));
async function createLease(){
  const created=await api(`/projects/${access.project_id}/builds`,{ref:"v-fixture-1",goal:"Controlled planning protocol fixture",target_requirements:[{os:platform.os,arch:platform.arch}]});
  builds.push(created.id);
  for(let i=0;i<120;i++){
    const task=await call("Poll",{});
    if(task.run_id){assert.equal(JSON.parse(task.context_json).build_id,created.id);return {task,build:created.id};}
    await delay(250);
  }
  throw new Error("planning lease unavailable");
}
const plan={summary:"Controlled valid boundary plan",evidence:[{path:"README.md",reason:"Native C build instructions"}],scripts:[],recipe:{version:1,targets:{native:{os:"darwin",arch:"arm64",timeout_seconds:60,steps:[{name:"compile",run:"mkdir -p dist; cc main.c modules/library/lib.c modules/library/nested/base/base.c -o dist/app",timeout_seconds:60}],artifacts:["dist/app"]}}}};
plan.recipe.targets.native.os=platform.os;plan.recipe.targets.native.arch=platform.arch;
const tool=(task,p)=>call("Tool",{run_id:task.run_id,lease_token:task.lease_token,name:"submit_plan",arguments_json:JSON.stringify({plan:p})}).then(r=>JSON.parse(r.result_json));
async function rejected(task,p,name){const value=await tool(task,p);check(name,!!value.error);}
try{
  for(const w of workerStates)await api("/ai-workers/"+w.id,{draining:true,revoked:w.revoked},"PATCH");
  for(const n of nodeStates)await api("/nodes/"+n.id,{...Object.fromEntries(["revoked","slots","labels","signing_profiles","pool_id"].map(k=>[k,n[k]])),draining:true},"PATCH");
  const token=(await api("/ai-enrollment-tokens",{})).token;
  execFileSync(process.execPath,[resolve(import.meta.dirname,"../dist/main.js"),"enroll","--config",path],{input:token,stdio:["pipe","pipe","pipe"]});
  const identity=JSON.parse(readFileSync(path));probeWorker=identity.worker_id;
  client=new definitions.Planning(config.endpoint,grpc.credentials.createSsl(readFileSync(config.ca_file),readFileSync(config.key_file),readFileSync(config.certificate_file)));
  let {task,build}=await createLease();
  const unauthorized=structuredClone(plan);unauthorized.recipe.targets.native.signing_profiles=["not-authorized"];
  unauthorized.recipe.targets.native.steps.push({name:"sign",phase:"sign",run:"true",timeout_seconds:30},{name:"verify",phase:"verify",run:"true",timeout_seconds:30});
  await rejected(task,unauthorized,"unauthorized signing plan rejected");
  const missing=structuredClone(plan);missing.evidence[0].path="nonexistent-build-file";
  await rejected(task,missing,"fabricated file evidence rejected");
  const extra=structuredClone(plan);extra.recipe.targets.other={...extra.recipe.targets.native,os:platform.os==="linux"?"darwin":"linux"};
  await rejected(task,extra,"unrequested platform rejected");
  // Inject a failure after plan_revisions INSERT to prove savepoint rollback.
  sql(`CREATE FUNCTION boundary_reject_job() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.build_id='${build}'::uuid THEN RAISE EXCEPTION 'controlled job insert failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER boundary_reject_job BEFORE INSERT ON jobs FOR EACH ROW EXECUTE FUNCTION boundary_reject_job();`);
  try{await rejected(task,plan,"late database failure rejects plan");}
  finally{sql("DROP TRIGGER boundary_reject_job ON jobs; DROP FUNCTION boundary_reject_job();");}
  check("rejected plan leaves no revision or jobs",sql(`SELECT (SELECT count(*) FROM plan_revisions WHERE build_id='${build}')+(SELECT count(*) FROM jobs WHERE build_id='${build}')`)==="0");
  check("valid plan accepted after rollback",(await tool(task,plan)).accepted);
  await assert.rejects(()=>tool(task,plan),error=>error.code===grpc.status.FAILED_PRECONDITION);check("duplicate plan cannot enqueue again",true);
  await api("/builds/"+build+"/cancel",{});

  ({task,build}=await createLease());
  sql(`UPDATE planning_runs SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE id='${task.run_id}'`);
  await assert.rejects(()=>tool(task,plan),error=>error.code===grpc.status.FAILED_PRECONDITION);check("expired lease submission rejected",true);
  await api("/builds/"+build+"/cancel",{});

  ({task,build}=await createLease());
  await api("/builds/"+build+"/cancel",{});
  await assert.rejects(()=>tool(task,plan),error=>error.code===grpc.status.FAILED_PRECONDITION);check("canceled planning cannot enqueue",true);
  check("canceled planning has no jobs",(await api("/builds/"+build)).jobs.length===0);

  ({task,build}=await createLease());
  let lock=await lockBuild(build);
  let cancellation=api("/builds/"+build+"/cancel",{});await blocked(lock.backend,1);
  let submission=tool(task,plan).catch(error=>error);await blocked(lock.backend,2);
  await lock.release();await cancellation;
  check("cancel wins a real concurrent submit lock race",(await submission).code===grpc.status.FAILED_PRECONDITION && (await api("/builds/"+build)).jobs.length===0);

  ({task,build}=await createLease());
  lock=await lockBuild(build);
  submission=tool(task,plan).catch(error=>error);await blocked(lock.backend,1);
  cancellation=api("/builds/"+build+"/cancel",{});await blocked(lock.backend,2);
  await lock.release();check("plan can commit before concurrent cancellation",(await submission).accepted);
  await cancellation;
  const canceled=await api("/builds/"+build);
  check("cancel after plan commit cancels all queued work",canceled.cancel_requested && canceled.jobs.every(j=>j.status==="canceled" && j.attempts.length===0));

  ({task,build}=await createLease());
  for(let i=0;i<task.max_calls;i++)await call("Report",{run_id:task.run_id,lease_token:task.lease_token,kind:"model_call",payload_json:"{}"});
  await assert.rejects(()=>call("Report",{run_id:task.run_id,lease_token:task.lease_token,kind:"model_call",payload_json:"{}"}),error=>error.code===grpc.status.RESOURCE_EXHAUSTED);
  check("server enforces model call budget",true);
  await api("/ai-workers/"+probeWorker,{draining:true,revoked:true},"PATCH");
  await assert.rejects(()=>tool(task,plan),error=>error.code===grpc.status.UNAUTHENTICATED);check("revoked worker rejected",true);
  await api("/builds/"+build+"/cancel",{});
}finally{
  client?.close();
  for(const build of builds)try{await api("/builds/"+build+"/cancel",{});}catch{}
  if(probeWorker)await api("/ai-workers/"+probeWorker,{draining:true,revoked:true},"PATCH");
  for(const w of workerStates)await api("/ai-workers/"+w.id,{draining:w.draining,revoked:w.revoked},"PATCH");
  for(const n of nodeStates)await api("/nodes/"+n.id,Object.fromEntries(["draining","revoked","slots","labels","signing_profiles","pool_id"].map(k=>[k,n[k]])),"PATCH");
  writeFileSync(resolve(run,"planning-boundary-evidence.json"),JSON.stringify(evidence,null,2));
}
