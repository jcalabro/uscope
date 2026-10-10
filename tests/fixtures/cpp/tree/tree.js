// @ts-check
// Draws a binary tree kept in an array: the first `count` of `nodes`, each
// a key and the indices of its children (-1 for none), from `root`. Each
// node sits at its depth, and as far across as its place in key order
// (Knuth's layout), so a search tree reads left to right in order. Nodes
// added since the stop before are outlined, and each opens its record.

const GAP = 34;
const LEVEL = 46;
const RADIUS = 13;

uscope.draw(({ nodes, count, root }, { previous, paths, width }) => {
  const theme = uscope.theme;
  if (count > nodes.length) throw new RangeError(`count is ${count}, past the ${nodes.length} nodes`);
  const order = [];
  const depth = new Map();
  // An in-order walk without recursion, which also finds each node's depth
  // and refuses a cycle.
  const stack = [];
  let at = root;
  let level = 0;
  const seen = new Set();
  while (at >= 0 || stack.length > 0) {
    while (at >= 0) {
      if (seen.has(at)) throw new RangeError(`node ${at} is reached twice: the children form a cycle`);
      if (at >= count) throw new RangeError(`a child index, ${at}, is past the ${count} nodes`);
      seen.add(at);
      depth.set(at, level);
      stack.push([at, level]);
      at = nodes[at].left;
      level += 1;
    }
    const [node, nodeLevel] = /** @type {[number, number]} */ (stack.pop());
    order.push(node);
    at = nodes[node].right;
    level = nodeLevel + 1;
  }
  const added = previous === null ? new Set() : new Set(order.filter((node) => node >= previous.count));
  const deepest = Math.max(0, ...depth.values());
  // Nodes spread to fill the card, up to half again their least gap.
  const spacing = Math.min(GAP * 1.5, Math.max(GAP, width / (order.length + 1)));
  const W = (order.length + 1) * spacing;
  const place = new Map(order.map((node, k) => [node, { x: spacing * (k + 1), y: 24 + (depth.get(node) ?? 0) * LEVEL }]));
  const shapes = [];
  for (const node of order) {
    const from = /** @type {{x: number, y: number}} */ (place.get(node));
    for (const child of [nodes[node].left, nodes[node].right]) {
      const to = place.get(child);
      if (to !== undefined) shapes.push(uscope.line({ x1: from.x, y1: from.y, x2: to.x, y2: to.y, stroke: theme.ink3, strokeWidth: 1.5 }));
    }
  }
  for (const node of order) {
    const { x, y } = /** @type {{x: number, y: number}} */ (place.get(node));
    const isNew = added.has(node);
    shapes.push(uscope.group({
      x, y,
      title: `${nodes[node].key}: node ${node}, depth ${depth.get(node)}`,
      select: paths.nodes === null ? undefined : `${paths.nodes}[${node}]`,
      shapes: [
        uscope.circle({ x: 0, y: 0, r: RADIUS, fill: theme.surface, stroke: isNew ? theme.changed : theme.series[0], strokeWidth: isNew ? 3 : 2 }),
        uscope.text({ x: 0, y: 0, text: String(nodes[node].key), size: 11, anchor: "middle", baseline: "central", fill: theme.ink }),
      ],
    }));
  }
  const unreached = count - order.length;
  const caption = [`${count} nodes`, `depth ${deepest + 1}`];
  if (unreached > 0) caption.push(`${unreached} not reached from the root`);
  if (added.size > 0) caption.push(`${added.size} added since the stop before`);
  return uscope.picture({ width: W, height: 24 + (deepest + 1) * LEVEL, shapes, caption: caption.join(" · ") });
});
