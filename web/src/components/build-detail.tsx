"use client";

import Link from "next/link";
import { useEffect, useRef, useState } from "react";
import { ArrowLeft, Clock3, GitBranch, GitCommitHorizontal, Pause, Play, RefreshCw, Server, ShieldCheck, Square, Terminal } from "lucide-react";
import { ArtifactTable, CopyButton, Empty, ErrorBox, Loading, PageTitle, SectionTitle, Status, StepIcon } from "@/components/common";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Modal } from "@/components/ui/dialog";
import { SubmitForm } from "@/components/forms";
import { Field } from "@/components/common";
import { mutation, useRemote } from "@/lib/api";
import type { Build, User } from "@/lib/types";
import { cn, date, elapsed, short } from "@/lib/utils";

interface Log { id: number; attempt_id: string; step: string; text: string }

export function BuildDetail({ id, user }: { id: string; user: User }) {
  const remote=useRemote<Build>(`/builds/${id}`); const build=remote.data;
  const [logs,setLogs]=useState<Log[]>([]); const [selected,setSelected]=useState<string>(); const [filter,setFilter]=useState<string>(); const [follow,setFollow]=useState(true); const [connected,setConnected]=useState(false); const [error,setError]=useState<string>(); const [publish,setPublish]=useState(false); const [clipped,setClipped]=useState(false); const terminal=useRef<HTMLDivElement>(null);
  useEffect(() => { setLogs([]); setSelected(undefined); setFilter(undefined); setClipped(false); const stream=new EventSource(`/api/v1/builds/${id}/events`); stream.addEventListener("snapshot",event => {remote.setData(JSON.parse(event.data)); setConnected(true);}); stream.addEventListener("log",event => {const log:Log=JSON.parse(event.data); setLogs(previous => {if(previous.some(entry=>entry.id===log.id)) return previous; if(previous.length>=3000) setClipped(true); return [...previous,log].slice(-3000);});}); stream.onerror=()=>setConnected(false); return ()=>stream.close(); },[id,remote.setData]);
  useEffect(() => {if(follow && terminal.current) terminal.current.scrollTop=terminal.current.scrollHeight;},[logs,follow,filter,selected]);
  async function action(path: string, body: unknown = {}) { setError(undefined); try {await mutation(path,body); await remote.reload();} catch(error) {setError(error instanceof Error ? error.message : "操作失败");} }
  if(!build) return <><ErrorBox message={remote.error}/><Loading/></>;
  const job=build.jobs?.find(job=>job.id===selected) || build.jobs?.[0];
  const attempt=job?.attempts.at(-1);
  const displayed=logs.filter(log=>(!attempt || log.attempt_id===attempt.id) && (!filter || log.step===filter));
  const active=["resolving","queued","running","canceling"].includes(build.status);
  return <>
    <Link href="/builds" className="mb-5 inline-flex items-center gap-2 text-xs text-muted-foreground hover:text-primary"><ArrowLeft size={13}/>全部构建</Link>
    <PageTitle eyebrow={`BUILD / ${short(build.id)}`} title={build.project_name} description={`构建结果与发布结果独立记录 · ${date(build.created_at)}`}>
      <Status value={build.status}/>{user.role!=="viewer" && active && <Button variant="destructive" onClick={()=>void action(`/builds/${id}/cancel`)}><Square/>取消构建</Button>}
      {user.role!=="viewer" && build.status==="succeeded" && build.publish_status!=="succeeded" && <Button onClick={()=>setPublish(true)}><ShieldCheck/>发布制品</Button>}
    </PageTitle>
    <ErrorBox message={error || remote.error || build.error}/>
    <div className="build-meta"><span className="meta-item"><GitBranch size={14}/>{build.requested_ref}</span><span className="meta-item"><GitCommitHorizontal size={14}/>{build.commit_sha || "等待解析"}</span><span className="meta-item"><Clock3 size={14}/>{elapsed(build.created_at,build.finished_at)}</span><span className="meta-item">RELEASE <Status small value={build.publish_status}/></span>{build.release_version && <span className="meta-item text-primary">{build.release_version}</span>}</div>
    <div className="build-layout">
      <section className="panel self-start"><SectionTitle title="目标矩阵" detail={`${build.jobs?.length || 0} targets`}/><div className="build-targets">{build.jobs?.length ? build.jobs.map(target=><button key={target.id} className={cn("target-button",job?.id===target.id && "selected")} onClick={()=>{setSelected(target.id);setFilter(undefined);}}><StepIcon status={target.status}/><div className="min-w-0 flex-1"><div className="font-mono text-xs">{target.target}</div><div className="mt-1.5 font-mono text-[10px] text-muted-foreground">{target.target_config.os} / {target.target_config.arch}</div></div><Status small value={target.status}/></button>) : <div className="p-5 text-xs leading-6 text-muted-foreground">{build.status==="failed" ? "配置解析未完成，未创建任务。" : "正在读取指定提交的配置…"}</div>}</div><div className="border-t border-border p-4 text-[11px] leading-6 text-muted-foreground">{attempt ? <><div className="flex items-center gap-2"><Server size={13}/>{attempt.node_name}</div><div className="mt-2 font-mono">attempt {short(attempt.id)}</div><div>租约截止 {date(attempt.lease_expires_at)}</div></> : "等待匹配具备能力的授权节点。"}</div></section>
      <div className="min-w-0 space-y-5"><section className="panel"><SectionTitle title="执行步骤" detail="BUILD → SIGN → VERIFY"/>{job?.target_config.steps.map((step,index)=>{const state=attempt?.steps.find(entry=>entry.step_index===index); return <button key={index} className={cn("flex w-full items-center gap-3 border-b border-border px-5 py-3.5 text-left last:border-0 hover:bg-secondary",filter===step.name && "bg-secondary")} onClick={()=>setFilter(filter===step.name ? undefined : step.name)}><StepIcon status={state?.status}/><span className="min-w-0 flex-1 text-xs">{step.name}</span><span className="tag">{step.phase}</span><span className="w-16 text-right font-mono text-[10px] text-muted-foreground">{state ? elapsed(state.started_at,state.finished_at) : "—"}</span></button>;})}{attempt?.error && <div className="p-4"><ErrorBox message={attempt.error}/></div>}{job && ["failed","lost","canceled"].includes(job.status) && user.role!=="viewer" && <div className="p-4"><Button size="sm" variant="secondary" onClick={()=>void action(`/jobs/${job.id}/retry`)}><RefreshCw/>以新 attempt 重试此目标</Button></div>}</section>
        <section className="panel"><SectionTitle live title={filter ? `日志 · ${filter}` : "执行日志"} detail={connected ? "SSE 已连接" : "等待连接 / 重连中"}><div className="flex items-center gap-1"><Button size="icon" variant="ghost" aria-label={follow ? "暂停跟随日志" : "跟随日志"} onClick={()=>setFollow(!follow)}>{follow ? <Pause/> : <Play/>}</Button><CopyButton value={displayed.map(log=>log.text).join("")}/></div></SectionTitle>{clipped && <div className="border-b border-border px-5 py-2 text-[10px] text-primary">浏览器仅保留最近 3000 个日志块；服务端日志未删除。</div>}<div className="log-console" ref={terminal} role="log" aria-label="构建日志" aria-live="off">{displayed.length ? displayed.map(log=><span key={log.id}>{!filter && <span className="mr-3 select-none text-[#687c6d]">[{log.step}]</span>}{log.text}</span>) : <span className="flex items-center gap-2 text-muted-foreground"><Terminal size={13}/>{active ? "等待 Worker 上传日志…" : "没有可显示的日志。"}</span>}</div></section>
      </div>
    </div>
    <section className="panel mt-6"><SectionTitle title="校验后的制品" detail="SHA-256 VERIFIED"/>{build.artifacts?.length ? <ArtifactTable artifacts={build.artifacts}/> : <Empty title="尚无已验证制品" detail="只有通过服务端大小和 SHA-256 校验的对象才会出现在这里。"/>}</section>
    {build.publish_error && <div className="mt-5"><ErrorBox message={`发布失败（构建结果保留）：${build.publish_error}`}/></div>}
    <details className="panel mt-5"><summary className="cursor-pointer px-5 py-4 text-xs text-muted-foreground">查看本次配置快照</summary><pre className="log-console min-h-0">{build.config_yaml || "未读取配置"}</pre></details>
    <Modal open={publish} onOpenChange={setPublish} title="发布已验证制品" description="复用本次构建的制品，不重新编译或签名。已发布的同名版本不会被覆盖。"><SubmitForm label="生成内部 Release" onSubmit={async form=>{await mutation(`/builds/${id}/publish`,{version:String(form.get("version"))});setPublish(false);await remote.reload();}}><Field label="版本名称"><Input name="version" defaultValue={build.release_version || ""} readOnly={!!build.release_version} required maxLength={100} placeholder="v1.0.0"/></Field></SubmitForm></Modal>
  </>;
}
