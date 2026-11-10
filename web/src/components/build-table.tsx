"use client";

import Link from "next/link";
import { ArrowUpRight, GitBranch, GitCommitHorizontal } from "lucide-react";
import { Empty, Status } from "@/components/common";
import { date, elapsed, short } from "@/lib/utils";
import type { Build } from "@/lib/types";

export function BuildTable({ builds }: { builds: Build[] }) {
  if (!builds.length) return <Empty title="构建队列还是空的" detail="接入一个 Git 项目，然后从指定提交发起第一次构建。任务只会分配给具备实际能力的授权节点。"/>;
  return <div className="table-scroll"><table><thead><tr><th>项目 / 构建</th><th>源码</th><th>构建状态</th><th>发布状态</th><th>耗时</th><th>创建时间</th><th/></tr></thead><tbody>{builds.map(build => <tr key={build.id}><td><Link href={`/builds/${build.id}`} className="font-medium hover:text-primary">{build.project_name}</Link><div className="mt-1.5 font-mono text-[10px] text-muted-foreground">#{short(build.id)}{build.release_version && <span className="ml-2 text-primary">{build.release_version}</span>}</div></td><td><div className="flex items-center gap-1.5 text-xs"><GitBranch size={12}/>{build.requested_ref}</div><div className="mt-1.5 flex items-center gap-1.5 font-mono text-[10px] text-muted-foreground"><GitCommitHorizontal size={12}/>{short(build.commit_sha)}</div></td><td><Status value={build.status}/></td><td><Status value={build.publish_status}/></td><td className="font-mono text-xs text-muted-foreground">{elapsed(build.created_at,build.finished_at)}</td><td className="font-mono text-[11px] text-muted-foreground">{date(build.created_at)}</td><td><Link className="inline-flex rounded p-1 text-muted-foreground hover:text-primary" href={`/builds/${build.id}`} aria-label={`查看构建 ${short(build.id)}`}><ArrowUpRight size={16}/></Link></td></tr>)}</tbody></table></div>;
}
