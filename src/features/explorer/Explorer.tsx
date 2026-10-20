import { useVirtualizer } from '@tanstack/react-virtual';
import { useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from 'react';
import { useShallow } from 'zustand/react/shallow';

import type { Node } from '@/bindings/Node';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { TableStructure } from '@/bindings/TableStructure';
import type { TemplateKind } from '@/bindings/TemplateKind';
import { comparesData, dataCompareAvailable, openDataCompare } from '@/features/datacompare/entry';
import { CreateTableDialog, StructureEditDialog, type StructureEdit } from '@/features/schema/forms';
import { TableActionDialog, type TableAction } from '@/features/schema/TableActionDialog';
import { openBackup } from '@/features/transfer/backup';
import { Icon, IconButton, Input, Menu, Skeleton, useContextMenu, type MenuEntry } from '@/components/ui';
import { t } from '@/i18n';
import { copyText } from '@/lib/clipboard';
import { formatCount } from '@/lib/format';
import { api } from '@/lib/ipc';
import { isMac } from '@/lib/platform';
import { isRelation, KIND_LABEL, KIND_MARK, objectName } from '@/lib/objects';
import { nodeKey, useExplorer } from '@/stores/explorer';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Explorer.module.css';
import { bulkTargets, clickSelection, extendSelection, selectSiblings, toggleKey, visibleSelection } from './selection';
import { TREE_PADDING, TREE_ROW_HEIGHT } from './tree';

type Item =
  | { type: 'node'; node: Node; key: string; depth: number }
  | { type: 'loading'; key: string; depth: number }
  | { type: 'error'; key: string; depth: number; parent: Node | null; message: string }
  | { type: 'empty'; key: string; depth: number };
type NodeItem = Extract<Item, { type: 'node' }>;

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
  const { expanded, selected, selection, filter, toggle, refresh, select, setSelection, setFilter } = useExplorer(
    useShallow((s) => ({ expanded: s.expanded, selected: s.selected, selection: s.selection, filter: s.filter, toggle: s.toggle, refresh: s.refresh, select: s.select, setSelection: s.setSelection, setFilter: s.setFilter })),
  );
  const session = useWorkspace((s) => s.session);
  const { openTableTab, openQueryTab, openContainerTab } = useWorkspace(useShallow((s) => ({ openTableTab: s.openTableTab, openQueryTab: s.openQueryTab, openContainerTab: s.openContainerTab })));
  const toast = useUi((s) => s.toast);
  const navigate = useUi((s) => s.navigate);
  const items = useVisibleItems();
  const tree = useRef<HTMLDivElement>(null);
  const context = useContextMenu<Node>();
  const [creating, setCreating] = useState<Node | null>(null);
  const [editing, setEditing] = useState<{ node: Node; edit: StructureEdit; structure: TableStructure } | null>(null);
  const [tableAction, setTableAction] = useState<{ action: TableAction; objects: Node[] } | null>(null);
  const nodes = useMemo(() => items.filter((i): i is NodeItem => i.type === 'node'), [items]);
  const order = useMemo(() => nodes.map((n) => n.key), [nodes]);
  const selectedKeys = useMemo(() => new Set(visibleSelection(selection, order)), [selection, order]);
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => tree.current,
    estimateSize: () => TREE_ROW_HEIGHT,
    getItemKey: (i) => items[i]?.key ?? i,
    scrollMargin: TREE_PADDING,
    overscan: 12,
  });

  /** Relations open their data; other leaves open their definition; containers expand. */
  const activate = (node: Node) => {
    select(nodeKey(node));
    if (isRelation(node)) openTableTab(node, 'data');
    else if (node.expandable) toggle(node);
    else openTableTab(node, 'ddl');
  };

  const sid = session ? { sessionId: session.session_id } : null;
  const caps = session?.capabilities;

  /** The container row above `key`, refreshed after its children change. */
  const parentOf = (key: string): Node | null => {
    const index = nodes.findIndex((n) => n.key === key);
    const depth = nodes[index]?.depth ?? 0;
    return nodes.slice(0, Math.max(index, 0)).reverse().find((n) => n.depth < depth && n.node.expandable && !isRelation(n.node))?.node ?? null;
  };

  /** Refreshes each distinct container of `objects` once (row counts, renamed, new or dropped objects). */
  const refreshParents = (objects: Node[]) => {
    const parents = new Map<string, Node | null>();
    for (const o of objects) {
      const parent = parentOf(nodeKey(o));
      parents.set(nodeKey(parent), parent);
    }
    parents.forEach((parent) => refresh(parent));
  };

  /** ⌘/Ctrl- and Shift-clicks change the selection; a plain click opens the row. */
  const onRowClick = (e: MouseEvent, item: NodeItem) => {
    const toggleMod = isMac ? e.metaKey : e.ctrlKey;
    if (!toggleMod && !e.shiftKey) {
      activate(item.node);
      return;
    }
    setSelection(clickSelection(selection, item.key, order, { toggle: toggleMod, range: e.shiftKey }), item.key);
  };

  /** Menu entry for a table action, disabled with the engine's reason when it cannot run. */
  const tableActionEntry = (action: TableAction, label: string, objects: Node[], danger = true): MenuEntry => {
    const support = action === 'drop' ? null : session?.table_actions[action];
    const reason =
      support && !support.available
        ? (support.note ?? 'Not supported by this engine.')
        : action !== 'drop' && objects.some((o) => o.kind !== 'table')
          ? 'Only tables can be truncated, emptied or duplicated.'
          : undefined;
    return { label, danger, disabled: reason !== undefined, title: reason, onSelect: () => setTableAction({ action, objects }) };
  };

  /** Actions on a multi-selection: copy names and, for relations of one database, truncate / empty / drop. */
  const bulkActions = (targets: Node[]): MenuEntry[] => {
    const copyNames = async () => {
      if (!sid) return;
      await copyText(Promise.all(targets.map((t) => api.qualifiedName(sid, t))).then((names) => names.join('\n')));
      toast(`${targets.length} names copied`);
    };
    const oneDatabase = targets.every((t) => t.database === targets[0]?.database);
    const writable = targets.every(isRelation) && oneDatabase && session?.documents == null && !session?.server.read_only;
    const n = targets.length;
    return [
      { type: 'label', label: `${n} selected` },
      { label: 'Copy names', onSelect: () => void copyNames() },
      ...(writable
        ? [
            { type: 'separator' as const },
            tableActionEntry('truncate', `Truncate ${n} tables…`, targets),
            tableActionEntry('empty', `Empty ${n} tables…`, targets),
            tableActionEntry('drop', `Drop ${n} objects…`, targets),
          ]
        : []),
    ];
  };

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
    const bulk = bulkTargets(selection, nodeKey(node), order, (k) => nodes.find((n) => n.key === k)?.node);
    if (bulk) return bulkActions(bulk);
    const copyName = async () => {
      if (!sid) return;
      await copyText(api.qualifiedName(sid, node));
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
        ...(caps?.backup
          ? [
              { type: 'separator' as const },
              { label: t('action.backup'), onSelect: () => openBackup('backup', node.database, schema) },
              ...(session?.server.read_only ? [] : [{ label: t('action.restore'), danger: true, onSelect: () => openBackup('restore', node.database) }]),
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
      ...(table && session && dataCompareAvailable() && comparesData(session.flavor)
        ? [
            {
              label: 'Compare data with…',
              onSelect: () => openDataCompare({ connection_id: session.connection.id, database: node.database, schema: node.schema, table: node.name }),
            },
          ]
        : []),
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
            tableActionEntry('duplicate', 'Duplicate table…', [node], false),
            { type: 'separator' as const },
            tableActionEntry('truncate', 'Truncate table…', [node]),
            tableActionEntry('empty', 'Empty table…', [node]),
            tableActionEntry('drop', 'Drop table…', [node]),
          ]
        : []),
      ...(!table && !session?.server.read_only
        ? [{ type: 'separator' as const }, tableActionEntry('drop', `Drop ${KIND_LABEL[node.kind].toLowerCase()}…`, [node])]
        : []),
    ];
  };

  const onKeyDown = (e: KeyboardEvent) => {
    const index = nodes.findIndex((n) => n.key === selected);
    const current = nodes[index]?.node;
    const go = (i: number, extend = false) => {
      const next = nodes[Math.max(0, Math.min(nodes.length - 1, i))];
      if (next) {
        if (extend) setSelection(extendSelection(selection, next.key, order), next.key);
        else select(next.key);
        virtualizer.scrollToIndex(items.indexOf(next), { align: 'auto' });
      }
    };
    const mod = isMac ? e.metaKey : e.ctrlKey;
    if (e.key === 'ArrowDown') go(index + 1, e.shiftKey);
    else if (e.key === 'ArrowUp') go(index - 1, e.shiftKey);
    else if (mod && e.key.toLowerCase() === 'a' && current && selected) {
      // Every visible row at the cursor's depth that is, like it, a relation (or not).
      const depth = nodes[index]?.depth;
      const relation = isRelation(current);
      const level = new Set(nodes.filter((n) => n.depth === depth && isRelation(n.node) === relation).map((n) => n.key));
      setSelection(selectSiblings(order, selected, (k) => level.has(k)), selected);
    } else if (e.key === ' ' && selected) setSelection(toggleKey(selection, selected, order), selected);
    else if (e.key === 'Escape' && selected && selectedKeys.size > 1) select(selected);
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

  /** One flattened row; the virtual wrapper around it carries the key and position. */
  const renderItem = (item: Item) => {
    const pad = 8 + item.depth * 16;
    if (item.type === 'loading')
      return (
        <div className={styles.skeleton} style={{ paddingLeft: pad + 16 }} aria-hidden>
          <Skeleton width="70%" />
        </div>
      );
    if (item.type === 'error')
      return (
        <div className={styles.error} style={{ marginLeft: pad }} role="alert">
          {item.message}
          <button type="button" className={styles.errorRetry} onClick={() => refresh(item.parent)}>
            Retry
          </button>
        </div>
      );
    if (item.type === 'empty')
      return (
        <div className={styles.empty} style={{ paddingLeft: pad + 16 }}>
          Empty
        </div>
      );
    const { node } = item;
    const open = Boolean(expanded[item.key]);
    const count = node.row_estimate;
    return (
      <div
        data-key={item.key}
        role="treeitem"
        aria-level={item.depth + 1}
        aria-expanded={node.expandable ? open : undefined}
        aria-selected={selectedKeys.has(item.key)}
        data-cursor={selected === item.key}
        aria-label={`${KIND_LABEL[node.kind]} ${objectName(node)}`}
        className={styles.node}
        data-container={node.expandable}
        data-current={node.current}
        style={{ paddingLeft: pad }}
        title={node.comment ?? undefined}
        onClick={(e) => onRowClick(e, item)}
        onContextMenu={(e) => {
          // Inside a multi-selection the menu acts on all of it.
          if (selectedKeys.size > 1 && selectedKeys.has(item.key)) setSelection(selection, item.key);
          else select(item.key);
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
        aria-multiselectable
        tabIndex={0}
        onKeyDown={onKeyDown}
        onFocus={() => !selected && nodes[0] && select(nodes[0].key)}
      >
        {/* Only rows near the viewport are in the DOM; the body keeps the full scroll height. */}
        <div className={styles.virtualBody} style={{ height: virtualizer.getTotalSize() }}>
          {virtualizer.getVirtualItems().map((row) => (
            // Measured, so taller rows (errors) get their real height.
            <div key={row.key} ref={virtualizer.measureElement} data-index={row.index} className={styles.virtualRow} style={{ transform: `translateY(${row.start - TREE_PADDING}px)` }}>
              {renderItem(items[row.index]!)}
            </div>
          ))}
        </div>
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
      {tableAction && session && (
        <TableActionDialog action={tableAction.action} objects={tableAction.objects} onClose={() => setTableAction(null)} onApplied={() => refreshParents(tableAction.objects)} />
      )}
      {editing && session && (
        <StructureEditDialog
          edit={editing.edit}
          structure={editing.structure}
          flavor={session.flavor}
          onClose={() => setEditing(null)}
          // Refresh the parent so renamed or dropped objects disappear.
          onApplied={() => refreshParents([editing.node])}
        />
      )}
    </aside>
  );
}

