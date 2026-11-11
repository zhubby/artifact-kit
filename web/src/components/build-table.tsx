"use client";

import { useUI } from "@/components/providers";

import Link from "next/link";
import { ArrowUpRight, GitBranch, GitCommitHorizontal } from "lucide-react";
import { Empty, Status } from "@/components/common";
import { elapsed, short } from "@/lib/utils";
import type { Build } from "@/lib/types";

export function BuildTable({ builds }: { builds: Build[] }) {
  const { t, date } = useUI();
  if (!builds.length)
    return <Empty title={t("builds.empty")} detail={t("builds.emptyDetail")} />;
  return (
    <div className="table-scroll">
      <table>
        <thead>
          <tr>
            <th>{t("builds.project")}</th>
            <th>{t("builds.source")}</th>
            <th>{t("builds.buildStatus")}</th>
            <th>{t("builds.publishStatus")}</th>
            <th>{t("builds.duration")}</th>
            <th>{t("common.createdAt")}</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {builds.map((build) => (
            <tr key={build.id}>
              <td>
                <Link
                  href={`/builds/${build.id}`}
                  className="font-medium hover:text-primary"
                >
                  {build.project_name}
                </Link>
                <div className="mt-1.5 font-mono text-[10px] text-muted-foreground">
                  #{short(build.id)}
                  {build.release_version && (
                    <span className="ml-2 text-primary">
                      {build.release_version}
                    </span>
                  )}
                </div>
              </td>
              <td>
                <div className="flex items-center gap-1.5 text-xs">
                  <GitBranch size={12} />
                  {build.requested_ref}
                </div>
                <div className="mt-1.5 flex items-center gap-1.5 font-mono text-[10px] text-muted-foreground">
                  <GitCommitHorizontal size={12} />
                  {short(build.commit_sha)}
                </div>
              </td>
              <td>
                <Status value={build.status} />
              </td>
              <td>
                <Status value={build.publish_status} />
              </td>
              <td className="font-mono text-xs text-muted-foreground">
                {elapsed(build.created_at, build.finished_at)}
              </td>
              <td className="font-mono text-[11px] text-muted-foreground">
                {date(build.created_at)}
              </td>
              <td>
                <Link
                  className="inline-flex rounded p-1 text-muted-foreground hover:text-primary"
                  href={`/builds/${build.id}`}
                  aria-label={t("builds.view", { id: short(build.id) })}
                >
                  <ArrowUpRight size={16} />
                </Link>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
