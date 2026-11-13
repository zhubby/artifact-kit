import {test} from "node:test";
import assert from "node:assert/strict";
import {fork} from "node:child_process";
import {once} from "node:events";

test("task shutdown abandons its lease without reporting planning failure", async () => {
  const child=fork(new URL("./task.js",import.meta.url),[],{stdio:["ignore","ignore","ignore","ipc"]});
  const reports:string[]=[];
  let reserved=false;
  child.on("message",(message:{type:string;id?:number;method?:string;payload?:{kind?:string}})=>{
    if(message.type!=="rpc")return;
    reports.push(message.payload?.kind||"");
    if(message.payload?.kind==="model_call"){
      reserved=true;
      child.send({type:"reply",id:message.id,result:{ok:true,wait_ms:60000}},()=>{
        child.send({type:"cancel"},()=>{});
      });
    }else child.send({type:"reply",id:message.id,result:{ok:true}},()=>{});
  });
  const exited=once(child,"exit");
  child.send({type:"start",task:{context:{goal:"Build"},model:{api:"openai-completions",base_url:"http://127.0.0.1:1",model:"test",api_key:"task-secret",parameters:{}},timeout_seconds:10,max_calls:20}},()=>{});
  const watchdog=setTimeout(()=>child.kill("SIGKILL"),5000);
  try{
    await exited;
    assert.ok(reserved,"the task reached the model-call reservation");
    assert.equal(reports.includes("failed"),false,"service shutdown must allow lease recovery");
  }finally{clearTimeout(watchdog);if(child.exitCode===null)child.kill();}
});
