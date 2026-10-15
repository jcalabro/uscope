// @ts-check
// Draws a chess position: 64 squares from a1 to h8, each empty or holding a piece.

const GLYPHS = {
  White: { King: "♔", Queen: "♕", Rook: "♖", Bishop: "♗", Knight: "♘", Pawn: "♙" },
  Black: { King: "♚", Queen: "♛", Rook: "♜", Bishop: "♝", Knight: "♞", Pawn: "♟" },
};
const SIZE = 40;

microscope.draw(({ squares, turn }, { previous, paths }) => {
  const shapes = [];
  for (let square = 0; square < 64; square++) {
    const file = square % 8;
    const rank = Math.floor(square / 8);
    const x = file * SIZE;
    const y = (7 - rank) * SIZE;
    const name = "abcdefgh"[file] + (rank + 1);
    const piece = squares[square];
    const moved = previous !== null && !microscope.same(previous.squares[square], piece);
    shapes.push(microscope.rect({
      x, y, width: SIZE, height: SIZE,
      fill: (file + rank) % 2 === 1 ? "#f0d9b5" : "#b58863",
      stroke: moved ? microscope.theme.changed : undefined,
      strokeWidth: 3,
      title: piece.variant === "Some"
        ? `${name}: ${piece.value.color.name} ${piece.value.kind.name}`
        : name,
      select: paths.squares === null ? undefined : `${paths.squares}[${square}]`,
    }));
    if (piece.variant === "Some") {
      const { color, kind } = piece.value;
      shapes.push(microscope.text({
        x: x + SIZE / 2, y: y + SIZE / 2, size: 32, anchor: "middle", baseline: "central",
        fill: "#111", text: GLYPHS[color.name][kind.name],
      }));
    }
  }
  return microscope.picture({ width: 8 * SIZE, height: 8 * SIZE, shapes, caption: `${turn.name} to move` });
});
