import { useMemo, useRef, type KeyboardEvent } from 'react';

import type { Node } from '@/bindings/Node';
import { Icon, IconButton, Input, Menu, Skeleton, useContextMenu, type MenuEntry } from '@/components/ui';
import { copyText } from '@/lib/clipboard';
import { formatCount } from '@/lib/format';
import { api } from '@/lib/ipc';
import { nodeKey, useExplorer } from '@/stores/explorer';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Explorer.module.css';

const KIND_MARK: Partial<Record<Node['kind'], string>> = { table: 'T', view: 'V', materialized_view: 'M' };
const KIND_LABEL: Record<Node['kind'], string> = { database: 'Database', schema: 'Schema', table: 'Table', view: 'View', materialized_view: 'Materialized view' };

type Item =
  | { type: 'node'; node: Node; key: string; depth: number }
  | { type: 'loading'; key: string; depth: number }
  | { type: 'error'; key: string; depth: number; parent: Node | null; message: string }
  | { type: 'empty'; key: string; depth: number };

function compactCount(n: number): string {
  if (n < 1000) return formatCount(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}K`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/** Flattens the expanded tree into visible rows, applying the name filter. */
function useVisibleItems(): Item[] {
  const { children, expanded, loading, errors, filter } = useExplorer();
  return useMemo(() => {
    const needle = filter.trim().toLowerCase();
    const out: Item[] = [];
    const matches = (node: Node): boolean => {
      if (!needle || node.name.toLowerCase().includes(needle)) return true;
      return (children[nodeKey(node)] ?? []).some(matches);
    };
    const walk = (parent: Node | null, depth: number) => {
      const key = nodeKey(parent);
      if (loading[key] && !children[key]) {
        for (let i = 0; i < 3; i += 1) out.push({ type: 'loading', key: `${key}:loading:${i}`, depth });
        return;
      }
      const error = errors[key];
      if (error) {
        out.push({ type: 'error', key: `${key}:error`, depth, parent, message: error.message });
        return;
      }
      const list = children[key] ?? [];
      if (list.length === 0 && children[key]) out.push({ type: 'empty', key: `${key}:empty`, depth });
      for (const node of list) {
        if (!matches(node)) continue;
        const k = nodeKey(node);
        out.push({ type: 'node', node, key: k, depth });
        if (node.expandable && (expanded[k] || (needle && children[k]))) walk(node, depth + 1);
      }
    };
    walk(null, 0);
    return out;
  }, [children, expanded, loading, errors, filter]);
}

export function Explorer({ width }: { width: number }) {
  const { expanded, selected, filter, toggle, refresh, select, setFilter } = useExplorer();
  const session = useWorkspace((s) => s.session);
  const { openTableTab, openQueryTab } = useWorkspace();
  const toast = useUi((s) => s.toast);
  const items = useVisibleItems();
  const tree = useRef<HTMLDivElement>(null);
  const context = useContextMenu<Node>();
  const nodes = items.filter((i): i is Extract<Item, { type: 'node' }> => i.type === 'node');

  const activate = (node: Node) => {
    select(nodeKey(node));
    if (node.expandable) toggle(node);
    else openTableTab(node, 'data');
  };

  const sid = session ? { sessionId: session.session_id } : null;

  const actions = (node: Node): MenuEntry[] => {
    const copyName = async () => {
      if (!sid) return;
      await copyText(await api.qualifiedName(sid, node));
      toast('Name copied');
    };
    if (node.kind === 'database' || node.kind === 'schema') {
      return [
        { label: 'Refresh', onSelect: () => refresh(node) },
        { label: 'Copy name', onSelect: () => void copyName() },
        { label: 'New query', onSelect: () => openQueryTab() },
      ];
    }
    return [
      { label: 'Open data', onSelect: () => openTableTab(node, 'data') },
      { label: 'Open structure', onSelect: () => openTableTab(node, 'structure') },
      { label: 'View DDL', onSelect: () => openTableTab(node, 'ddl') },
      { type: 'separator' },
      { label: 'Copy qualified name', onSelect: () => void copyName() },
      {
        label: 'Generate SELECT',
        onSelect: async () => {
          if (!sid) return;
          try {
            openQueryTab({ sql: await api.generateSelect(sid, node), title: node.name });
          } catch (e) {
            useUi.getState().toastError(e);
          }
        },
      },
      { type: 'separator' },
      { label: 'Generate INSERT / UPDATE / DELETE', disabled: true, hint: 'Planned · P1', onSelect: () => undefined },
    ];
  };

  const onKeyDown = (e: KeyboardEvent) => {
    const index = nodes.findIndex((n) => n.key === selected);
    const current = nodes[index]?.node;
    const go = (i: number) => {
      const next = nodes[Math.max(0, Math.min(nodes.length - 1, i))];
      if (next) {
        select(next.key);
        tree.current?.querySelector(`[data-key="${CSS.escape(next.key)}"]`)?.scrollIntoView({ block: 'nearest' });
      }
    };
    if (e.key === 'ArrowDown') go(index + 1);
    else if (e.key === 'ArrowUp') go(index - 1);
    else if (e.key === 'ArrowRight' && current?.expandable && !expanded[nodeKey(current)]) toggle(current);
    else if (e.key === 'ArrowLeft' && current?.expandable && expanded[nodeKey(current)]) toggle(current);
    else if (e.key === 'ArrowLeft' && current) {
      const depth = nodes[index]?.depth ?? 0;
      const parent = nodes.slice(0, index).reverse().find((n) => n.depth < depth);
      if (parent) go(nodes.indexOf(parent));
    } else if (e.key === 'Enter' && current) activate(current);
    else return;
    e.preventDefault();
  };

  return (
    <aside className={styles.explorer} style={{ width }} aria-label="Explorer">
      <div className={styles.head}>
        <div className={styles.headRow}>
          <span className="dq-eyebrow">Explorer</span>
          <IconButton label="Refresh databases" onClick={() => refresh(null)}>
            <Icon name="refresh" size={12} />
          </IconButton>
        </div>
        <Input label="Filter objects" placeholder="Filter objects" value={filter} onChange={(e) => setFilter(e.target.value)} />
      </div>
      <div
        ref={tree}
        className={styles.tree}
        role="tree"
        aria-label="Database objects"
        tabIndex={0}
        onKeyDown={onKeyDown}
        onFocus={() => !selected && nodes[0] && select(nodes[0].key)}
      >
        {items.map((item) => {
          const pad = 8 + item.depth * 16;
          if (item.type === 'loading')
            return (
              <div key={item.key} className={styles.skeleton} style={{ paddingLeft: pad + 16 }} aria-hidden>
                <Skeleton width="70%" />
              </div>
            );
          if (item.type === 'error')
            return (
              <div key={item.key} className={styles.error} style={{ marginLeft: pad }} role="alert">
                {item.message}
                <button type="button" className={styles.errorRetry} onClick={() => refresh(item.parent)}>
                  Retry
                </button>
              </div>
            );
          if (item.type === 'empty')
            return (
              <div key={item.key} className={styles.empty} style={{ paddingLeft: pad + 16 }}>
                Empty
              </div>
            );
          const { node } = item;
          const open = Boolean(expanded[item.key]);
          const count = node.row_estimate;
          return (
            <div
              key={item.key}
              data-key={item.key}
              role="treeitem"
              aria-level={item.depth + 1}
              aria-expanded={node.expandable ? open : undefined}
              aria-selected={selected === item.key}
              aria-label={`${KIND_LABEL[node.kind]} ${node.name}`}
              className={styles.node}
              data-container={node.expandable}
              data-current={node.current}
              style={{ paddingLeft: pad }}
              title={node.comment ?? undefined}
              onClick={() => activate(node)}
              onContextMenu={(e) => {
                select(item.key);
                context.open(e, node);
              }}
            >
              <span className={styles.chevron}>{node.expandable && <Icon name={open ? 'chevronDown' : 'chevronRight'} size={10} />}</span>
              <span className={styles.kind}>{KIND_MARK[node.kind] ?? ''}</span>
              <span className={styles.label}>{node.name}</span>
              {count !== null && count > 0 && (
                <span className={styles.count} title={node.kind === 'schema' ? `${count} tables and views` : `About ${formatCount(count)} rows (estimate)`}>
                  {node.kind === 'schema' ? formatCount(count) : `≈${compactCount(count)}`}
                </span>
              )}
            </div>
          );
        })}
        {items.length === 0 && <div className={styles.empty}>{filter ? 'No loaded objects match.' : ' '}</div>}
      </div>
      {context.menu && <Menu x={context.menu.x} y={context.menu.y} items={actions(context.menu.target)} onClose={context.close} label="Object actions" />}
    </aside>
  );
}

