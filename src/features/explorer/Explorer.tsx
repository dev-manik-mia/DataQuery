import { useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { useShallow } from 'zustand/react/shallow';

import type { Node } from '@/bindings/Node';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { TableStructure } from '@/bindings/TableStructure';
import type { TemplateKind } from '@/bindings/TemplateKind';
import { CreateTableDialog, StructureEditDialog, type StructureEdit } from '@/features/schema/forms';
import { Icon, IconButton, Input, Menu, Skeleton, useContextMenu, type MenuEntry } from '@/components/ui';
import { copyText } from '@/lib/clipboard';
import { formatCount } from '@/lib/format';
import { api } from '@/lib/ipc';
import { isRelation, KIND_LABEL, KIND_MARK, objectName } from '@/lib/objects';
import { nodeKey, useExplorer } from '@/stores/explorer';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Explorer.module.css';


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
  const { children, expanded, loading, errors, filter } = useExplorer(useShallow((s) => ({ children: s.children, expanded: s.expanded, loading: s.loading, errors: s.errors, filter: s.filter })));
  return useMemo(() => {
    const needle = filter.trim().toLowerCase();
    const out: Item[] = [];
    const matches = (node: Node): boolean => {
      if (!needle || objectName(node).toLowerCase().includes(needle)) return true;
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

/** What the generated templates are called per language: select, insert, update, delete. */
const TEMPLATE_WORDS: Partial<Record<SqlFlavor, [string, string, string, string]>> = {
  mongodb: ['find', 'insertOne', 'updateOne', 'deleteOne'],
  query_dsl: ['search', 'index request', 'update request', 'delete request'],
  cypher: ['MATCH', 'CREATE', 'SET', 'DETACH DELETE'],
};

export function Explorer({ width }: { width: number }) {
  const { expanded, selected, filter, toggle, refresh, select, setFilter } = useExplorer(useShallow((s) => ({ expanded: s.expanded, selected: s.selected, filter: s.filter, toggle: s.toggle, refresh: s.refresh, select: s.select, setFilter: s.setFilter })));
  const session = useWorkspace((s) => s.session);
  const { openTableTab, openQueryTab, openContainerTab } = useWorkspace(useShallow((s) => ({ openTableTab: s.openTableTab, openQueryTab: s.openQueryTab, openContainerTab: s.openContainerTab })));
  const toast = useUi((s) => s.toast);
  const navigate = useUi((s) => s.navigate);
  const items = useVisibleItems();
  const tree = useRef<HTMLDivElement>(null);
  const context = useContextMenu<Node>();
  const [creating, setCreating] = useState<Node | null>(null);
  const [editing, setEditing] = useState<{ node: Node; edit: StructureEdit; structure: TableStructure } | null>(null);
  const nodes = items.filter((i): i is Extract<Item, { type: 'node' }> => i.type === 'node');

  /** Relations open their data; other leaves open their definition; containers expand. */
  const activate = (node: Node) => {
    select(nodeKey(node));
    if (isRelation(node)) openTableTab(node, 'data');
    else if (node.expandable) toggle(node);
    else openTableTab(node, 'ddl');
  };

  const sid = session ? { sessionId: session.session_id } : null;
  const caps = session?.capabilities;

  const template = async (node: Node, kind: TemplateKind) => {
    if (!sid) return;
    try {
      openQueryTab({ sql: await api.generateSql(sid, node, kind), title: `${node.name} ${kind}` });
    } catch (e) {
      useUi.getState().toastError(e);
    }
  };

  const editTable = async (node: Node, edit: StructureEdit) => {
    if (!sid) return;
    try {
      setEditing({ node, edit, structure: await api.tableStructure(sid, node) });
    } catch (e) {
      useUi.getState().toastError(e);
    }
  };

  const actions = (node: Node): MenuEntry[] => {
    const copyName = async () => {
      if (!sid) return;
      await copyText(await api.qualifiedName(sid, node));
      toast('Name copied');
    };
    // The container of tables: the schema, or the database on engines without schemas.
    const isContainer = node.kind === 'schema' || (node.kind === 'database' && !caps?.schemas);
    // Document engines have no SQL schema: no create/rename/drop forms, ER diagram or migrations.
    const documents = session?.documents != null;
    const schema = node.kind === 'schema' ? node.name : null;
    if (node.kind === 'database' || node.kind === 'schema') {
      return [
        { label: 'Refresh', onSelect: () => refresh(node) },
        { label: 'Copy name', onSelect: () => void copyName() },
        { label: 'New query', onSelect: () => openQueryTab() },
        ...(isContainer && !documents && !session?.server.read_only
          ? [{ type: 'separator' as const }, { label: 'Create table…', onSelect: () => setCreating(node) }]
          : []),
        ...(isContainer && !documents
          ? [
              { type: 'separator' as const },
              { label: 'ER diagram', onSelect: () => openContainerTab('diagram', node.database, schema) },
              { label: 'Framework migrations', onSelect: () => openContainerTab('migrations', node.database, schema) },
              ...(caps?.schema_diff ? [{ label: 'Compare with… (schema diff)', onSelect: () => navigate({ name: 'diff' }) }] : []),
            ]
          : []),
      ];
    }
    if (node.kind === 'group') return [{ label: 'Refresh', onSelect: () => refresh(node) }];
    if (!isRelation(node)) {
      const owner = node.parent && (node.kind === 'index' || node.kind === 'trigger') ? node.parent : null;
      return [
        { label: 'Show definition', onSelect: () => openTableTab(node, 'ddl') },
        { label: 'Copy name', onSelect: () => void copyName() },
        ...(node.kind === 'index' && owner && !session?.server.read_only
          ? [
              { type: 'separator' as const },
              {
                label: 'Drop index…',
                danger: true,
                onSelect: () => void editTable({ ...node, kind: 'table', name: owner, parent: undefined }, { kind: 'drop-index', name: node.name }),
              },
            ]
          : []),
      ];
    }
    const table = node.kind === 'table';
    const writable = table && !session?.server.read_only;
    if (documents) {
      return [
        { label: 'Open documents', onSelect: () => openTableTab(node, 'data') },
        { type: 'separator' },
        { label: 'Copy name', onSelect: () => void copyName() },
        { label: `Generate ${TEMPLATE_WORDS[session?.flavor ?? 'postgres']?.[0] ?? 'SELECT'}`, onSelect: () => void template(node, 'select') },
        ...(table
          ? (['insert', 'update', 'delete'] as const).map((kind, i) => ({
              label: `Generate ${TEMPLATE_WORDS[session?.flavor ?? 'postgres']?.[i + 1] ?? kind.toUpperCase()}`,
              onSelect: () => void template(node, kind),
            }))
          : []),
      ];
    }
    return [
      { label: 'Open data', onSelect: () => openTableTab(node, 'data') },
      { label: 'Open structure', onSelect: () => openTableTab(node, 'structure') },
      { label: 'View DDL', onSelect: () => openTableTab(node, 'ddl') },
      { label: 'Dependencies', onSelect: () => openTableTab(node, 'dependencies') },
      { type: 'separator' },
      { label: 'Copy qualified name', onSelect: () => void copyName() },
      { label: 'Generate SELECT', onSelect: () => void template(node, 'select') },
      ...(table
        ? [
            { label: 'Generate INSERT', onSelect: () => void template(node, 'insert') },
            { label: 'Generate UPDATE', onSelect: () => void template(node, 'update') },
            { label: 'Generate DELETE', onSelect: () => void template(node, 'delete') },
          ]
        : []),
      ...(node.expandable ? [{ type: 'separator' as const }, { label: 'Refresh indexes and triggers', onSelect: () => refresh(node) }] : []),
      ...(writable
        ? [
            { type: 'separator' as const },
            { label: 'Rename table…', onSelect: () => void editTable(node, { kind: 'rename-table' }) },
            { label: 'Drop table…', danger: true, onSelect: () => void editTable(node, { kind: 'drop-table' }) },
          ]
        : []),
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
              aria-label={`${KIND_LABEL[node.kind]} ${objectName(node)}`}
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
              <span
                className={styles.chevron}
                onClick={(e) => {
                  if (!node.expandable) return;
                  e.stopPropagation();
                  select(item.key);
                  toggle(node);
                }}
              >
                {node.expandable && <Icon name={open ? 'chevronDown' : 'chevronRight'} size={10} />}
              </span>
              <span className={styles.kind}>{node.kind === 'group' ? <Icon name="folder" size={11} /> : KIND_MARK[node.kind]}</span>
              <span className={styles.label}>{objectName(node)}</span>
              {count !== null && count > 0 && (
                // Containers (schemas, and databases on engines without schemas) count objects; relations estimate rows.
                <span className={styles.count} title={isRelation(node) ? `About ${formatCount(count)} rows (estimate)` : `${count} objects`}>
                  {isRelation(node) ? `≈${compactCount(count)}` : formatCount(count)}
                </span>
              )}
            </div>
          );
        })}
        {items.length === 0 && <div className={styles.empty}>{filter ? 'No loaded objects match.' : ' '}</div>}
      </div>
      {context.menu && <Menu x={context.menu.x} y={context.menu.y} items={actions(context.menu.target)} onClose={context.close} label="Object actions" />}
      {creating && session && (
        <CreateTableDialog
          container={creating}
          flavor={session.flavor}
          onClose={() => setCreating(null)}
          onApplied={(table) => {
            refresh(creating);
            openTableTab(table, 'structure');
          }}
        />
      )}
      {editing && session && (
        <StructureEditDialog
          edit={editing.edit}
          structure={editing.structure}
          flavor={session.flavor}
          onClose={() => setEditing(null)}
          onApplied={() => {
            // Refresh the parent so renamed or dropped objects disappear.
            const parentKey = nodes.slice(0, nodes.findIndex((n) => n.key === nodeKey(editing.node))).reverse().find((n) => n.node.expandable && !isRelation(n.node));
            refresh(parentKey?.node ?? null);
          }}
        />
      )}
    </aside>
  );
}

