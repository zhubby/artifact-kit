import type { NextConfig } from "next";

const config: NextConfig = {
  output: "standalone",
  poweredByHeader: false,
  compress: false,
  async rewrites() {
    return [{ source: "/api/:path*", destination: `${process.env.ARTIFACTORY_API_URL || "http://127.0.0.1:8080"}/api/:path*` }];
  },
};

export default config;
