"use client";

import { useState, type FormEvent, type ReactNode } from "react";
import { useRouter } from "next/navigation";
import { CopyButton, ErrorBox, Field } from "@/components/common";
import { Button } from "@/components/ui/button";
import { Input, Textarea } from "@/components/ui/input";
import { Modal } from "@/components/ui/dialog";
import { mutation } from "@/lib/api";
import type { Credential, Node, Pool, Project } from "@/lib/types";

export function SubmitForm({ children, onSubmit, label = "保存", onCancel }: { children: ReactNode; onSubmit: (form: FormData) => Promise<void>; label?: string; onCancel?: () => void }) {
  const [error,setError]=useState<string>(); const [busy,setBusy]=useState(false);
  async function submit(event: FormEvent<HTMLFormElement>) { event.preventDefault(); const form=new FormData(event.currentTarget); setBusy(true); setError(undefined); try { await onSubmit(form); } catch(error) { setError(error instanceof Error ? error.message : "操作失败"); } finally { setBusy(false); } }
  return <form onSubmit={submit} className="form-stack"><ErrorBox message={error}/>{children}<div className="form-footer">{onCancel && <Button variant="secondary" type="button" onClick={onCancel}>取消</Button>}<Button type="submit" disabled={busy}>{busy ? "正在处理…" : label}</Button></div></form>;
}

const string = (form: FormData, name: string) => String(form.get(name) || "");
const csv = (value: string) => value.split(",").map(part => part.trim()).filter(Boolean);

export function ProjectForm({ pools, credentials, project, done }: { pools: Pool[]; credentials: Credential[]; project?: Project; done: () => void }) {
  return <SubmitForm onCancel={done} onSubmit={async form => { await mutation(project ? `/projects/${project.id}` : "/projects", { name:string(form,"name"),repository_url:string(form,"repository_url"),credential_id:string(form,"credential_id") || null,pool_ids:form.getAll("pools"),signing_profiles:csv(string(form,"signing_profiles")) },project ? "PATCH" : "POST"); done(); }}>
    <Field label="项目名称"><Input name="name" defaultValue={project?.name} required maxLength={100} placeholder="例如 Desktop Client"/></Field>
    <Field label="Git 仓库" hint="支持 HTTPS、ssh:// 或 git@host:path；SSH 必须配置已校验的 known_hosts。"><Input name="repository_url" defaultValue={project?.repository_url} required placeholder="https://git.example.com/team/app.git"/></Field>
    <Field label="只读 Git 凭据"><select name="credential_id" className="input" defaultValue={project?.credential_id || ""}><option value="">匿名访问 / 无凭据</option>{credentials.map(credential => <option value={credential.id} key={credential.id}>{credential.name} · {credential.kind}</option>)}</select></Field>
    <fieldset><legend className="mb-3 text-sm font-medium">授权节点池</legend><div className="flex flex-wrap gap-4">{pools.map(pool => <label key={pool.id} className="flex items-center gap-2 text-xs"><input type="checkbox" name="pools" value={pool.id} defaultChecked={project ? project.pool_ids.includes(pool.id) : pool.name==="default"}/>{pool.name}</label>)}</div></fieldset>
    <Field label="授权签名引用" hint="仅填写引用名称，逗号分隔。私钥保存在节点；还需在节点侧绑定相同引用。"><Input name="signing_profiles" defaultValue={project?.signing_profiles.join(", ")} placeholder="apple-team, windows-release"/></Field>
  </SubmitForm>;
}

export function BuildForm({ projects, projectID, done }: { projects: Project[]; projectID?: string; done: () => void }) {
  const router=useRouter();
  return <SubmitForm label="发起构建" onCancel={done} onSubmit={async form => { const result=await mutation<{id:string}>(`/projects/${string(form,"project_id")}/builds`,{ref:string(form,"ref"),parameters:JSON.parse(string(form,"parameters") || "{}"),targets:csv(string(form,"targets")),release_version:string(form,"release_version") || null}); done(); router.push(`/builds/${result.id}`); }}>
    <Field label="项目"><select name="project_id" className="input" required defaultValue={projectID || projects[0]?.id}>{projects.map(project => <option value={project.id} key={project.id}>{project.name}</option>)}</select></Field>
    <Field label="Git 引用" hint="中心会解析为确切提交，并使用同一提交的 artifact-kit.yaml。"><Input name="ref" defaultValue="HEAD" required placeholder="main / refs/tags/v1.0.0 / commit"/></Field>
    <Field label="目标平台" hint="逗号分隔的配置目标名。留空执行配置内全部目标。"><Input name="targets" placeholder="linux, macos, windows"/></Field>
    <Field label="构建参数 JSON" hint="仅允许配置已声明的字符串参数，通过 AK_PARAM_ 前缀环境变量传给步骤。"><Textarea name="parameters" defaultValue="{}" spellCheck={false}/></Field>
    <Field label="内部 Release 版本（可选）" hint="留空仅构建。填写后需配置启用 release，所有目标成功才会自动发布。"><Input name="release_version" maxLength={100} placeholder="例如 v1.0.0-preview.1"/></Field>
  </SubmitForm>;
}

export function NodeForm({ node, pools, done }: { node: Node; pools: Pool[]; done: () => void }) {
  return <SubmitForm onCancel={done} onSubmit={async form => { await mutation(`/nodes/${node.id}`,{draining:form.has("draining"),revoked:node.revoked,slots:Number(form.get("slots")),pool_id:string(form,"pool_id"),labels:JSON.parse(string(form,"labels")),signing_profiles:csv(string(form,"signing_profiles"))},"PATCH"); done(); }}>
    <Field label="节点池"><select className="input" name="pool_id" defaultValue={node.pool_id}>{pools.map(pool => <option value={pool.id} key={pool.id}>{pool.name}</option>)}</select></Field>
    <Field label="并发槽位" hint="默认 1。提高并发前需评估共享 SDK、签名钥匙串和磁盘空间。"><Input name="slots" type="number" min={1} max={32} defaultValue={node.slots} required/></Field>
    <Field label="管理员标签 JSON"><Textarea name="labels" defaultValue={JSON.stringify(node.labels,null,2)} spellCheck={false}/></Field>
    <Field label="授权签名引用" hint="必须与本机 Agent 配置中的 signing_profiles 对应；这里不会上传或创建私钥。"><Input name="signing_profiles" defaultValue={node.signing_profiles.join(", ")}/></Field>
    <label className="flex gap-2 text-sm"><input type="checkbox" name="draining" defaultChecked={node.draining}/>排空节点：不再分配新任务，保留在途任务</label>
  </SubmitForm>;
}

export function CredentialForm({ done }: { done: () => void }) {
  const [kind,setKind]=useState("https");
  return <SubmitForm onCancel={done} onSubmit={async form => { await mutation("/credentials",{name:string(form,"name"),kind,username:string(form,"username"),secret:string(form,"secret"),known_hosts:string(form,"known_hosts")}); done(); }}>
    <Field label="凭据名称"><Input name="name" required maxLength={100}/></Field>
    <Field label="认证方式"><select className="input" value={kind} onChange={event => setKind(event.target.value)}><option value="https">HTTPS · 用户名 / Token</option><option value="ssh">SSH · 私钥 / 主机指纹</option></select></Field>
    {kind==="https" && <Field label="Git 用户名"><Input name="username" autoComplete="off"/></Field>}
    <Field label={kind==="ssh" ? "只读部署私钥" : "密码或只读访问 Token"} hint="提交后仅保存加密内容，界面不会回显。不要在此填写应用签名私钥。">{kind==="ssh" ? <Textarea name="secret" required autoComplete="off" spellCheck={false}/> : <Input name="secret" type="password" required autoComplete="new-password"/>}</Field>
    {kind==="ssh" && <Field label="已验证的 known_hosts" hint="应通过可信渠道核对指纹，不能仅依赖未经核验的 ssh-keyscan 输出。"><Textarea name="known_hosts" required spellCheck={false}/></Field>}
  </SubmitForm>;
}

export function SecretDialog({ value, title, close }: { value?: string; title: string; close: () => void }) {
  return <Modal open={!!value} onOpenChange={open => {if(!open) close();}} title={title} description="该值仅显示一次。妥善保存，不要写入源代码、构建参数或任务日志。"><div className="flex items-start gap-2 rounded-md border border-border bg-background p-3"><code className="min-w-0 flex-1 break-all text-xs leading-6 text-primary">{value}</code><CopyButton value={value || ""}/></div><div className="mt-5 text-xs leading-6 text-muted-foreground">Agent 注册使用环境变量 AK_ENROLLMENT_TOKEN；节点注册令牌 15 分钟有效且仅能使用一次。</div><Button className="mt-6 w-full" onClick={close}>已安全保存</Button></Modal>;
}
