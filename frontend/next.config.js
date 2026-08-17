const path = require('path');
const tiptapPmResolveBase = path.dirname(require.resolve('@tiptap/pm/model'));
const resolveFromTiptapPm = (pkg) =>
  require.resolve(pkg, { paths: [tiptapPmResolveBase] });

/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: false, // Disabled for BlockNote compatibility
  output: 'export',

  // Strip chatty logging from release builds. There are ~400 console calls in
  // this app, many on per-poll and per-transcript paths, and each one is a real
  // string format plus a WebView2 IPC hop. error/warn are kept so genuine
  // failures still surface in DevTools.
  compiler: {
    removeConsole: {
      exclude: ['error', 'warn'],
    },
  },

  images: {
    unoptimized: true,
  },
  // Add basePath configuration
  basePath: '',
  assetPrefix: '/',

  // Add webpack configuration for Tauri
  webpack: (config, { isServer }) => {
    if (!isServer) {
      config.resolve.fallback = {
        ...config.resolve.fallback,
        fs: false,
        path: false,
        os: false,
      };

      // Keep ProseMirror single-instanced for BlockNote/Tiptap.
      config.resolve.alias = {
        ...config.resolve.alias,
        '@blocknote/core$': require.resolve('@blocknote/core'),
        '@blocknote/react$': require.resolve('@blocknote/react'),
        '@blocknote/shadcn$': require.resolve('@blocknote/shadcn'),
        'prosemirror-model': resolveFromTiptapPm('prosemirror-model'),
        'prosemirror-state': resolveFromTiptapPm('prosemirror-state'),
        'prosemirror-view': resolveFromTiptapPm('prosemirror-view'),
        'prosemirror-transform': resolveFromTiptapPm('prosemirror-transform'),
        'prosemirror-tables': resolveFromTiptapPm('prosemirror-tables'),
        'prosemirror-schema-list': resolveFromTiptapPm('prosemirror-schema-list'),
        'prosemirror-keymap': resolveFromTiptapPm('prosemirror-keymap'),
        'prosemirror-commands': resolveFromTiptapPm('prosemirror-commands'),
        'prosemirror-history': resolveFromTiptapPm('prosemirror-history'),
        'prosemirror-inputrules': resolveFromTiptapPm('prosemirror-inputrules'),
        'prosemirror-gapcursor': resolveFromTiptapPm('prosemirror-gapcursor'),
        'prosemirror-dropcursor': resolveFromTiptapPm('prosemirror-dropcursor'),
      };
    }
    return config;
  },
}

module.exports = nextConfig
