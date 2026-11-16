#!/usr/bin/env python3
"""Render the final visible screen from a raw PTY transcript.

This is a small ANSI terminal emulator for acceptance checks. It supports the
control sequences ratatui and crossterm emit in slowcode's TUI: cursor
positioning, erase, SGR, alt screen and printable text. It intentionally
ignores styling and scrollback.
"""

from __future__ import annotations

import sys
import unicodedata


def char_width(ch: str) -> int:
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


class Screen:
    def __init__(self, rows: int, cols: int) -> None:
        self.rows = rows
        self.cols = cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = 0
        self.col = 0

    def clear(self) -> None:
        self.grid = [[" "] * self.cols for _ in range(self.rows)]
        self.row = 0
        self.col = 0

    def scroll(self) -> None:
        self.grid.pop(0)
        self.grid.append([" "] * self.cols)
        self.row = self.rows - 1

    def erase_to_end(self) -> None:
        for col in range(self.col, self.cols):
            self.grid[self.row][col] = " "
        for row in range(self.row + 1, self.rows):
            self.grid[row] = [" "] * self.cols

    def erase_line_to_end(self) -> None:
        for col in range(self.col, self.cols):
            self.grid[self.row][col] = " "

    def put(self, ch: str) -> None:
        width = char_width(ch)
        if width == 0:
            if self.col > 0:
                self.grid[self.row][self.col - 1] += ch
            return
        if self.col + width > self.cols:
            self.row += 1
            self.col = 0
            if self.row >= self.rows:
                self.scroll()
        self.grid[self.row][self.col] = ch
        if width == 2 and self.col + 1 < self.cols:
            self.grid[self.row][self.col + 1] = ""
        self.col += width

    def newline(self) -> None:
        self.row += 1
        self.col = 0
        if self.row >= self.rows:
            self.scroll()

    def move(self, row: int | None = None, col: int | None = None) -> None:
        if row is not None:
            self.row = max(0, min(self.rows - 1, row))
        if col is not None:
            self.col = max(0, min(self.cols - 1, col))

    def render(self) -> str:
        return "\n".join("".join(cell for cell in row).rstrip() for row in self.grid)


def apply_csi(screen: Screen, params: str, final: str) -> None:
    private = params.startswith("?")
    if private:
        params = params[1:]
    values = [int(value) if value else 0 for value in params.split(";")] if params else []

    def value(index: int, default: int) -> int:
        return values[index] if index < len(values) else default

    if final in "Hf":
        screen.move(value(0, 1) - 1, value(1, 1) - 1)
    elif final == "G":
        screen.move(col=value(0, 1) - 1)
    elif final == "A":
        screen.move(row=screen.row - value(0, 1))
    elif final == "B":
        screen.move(row=screen.row + value(0, 1))
    elif final == "C":
        screen.move(col=screen.col + value(0, 1))
    elif final == "D":
        screen.move(col=screen.col - value(0, 1))
    elif final == "J":
        mode = value(0, 0)
        if mode in (2, 3):
            screen.clear()
        else:
            screen.erase_to_end()
    elif final == "K":
        mode = value(0, 0)
        if mode in (0,):
            screen.erase_line_to_end()
        elif mode == 2:
            for col in range(screen.cols):
                screen.grid[screen.row][col] = " "
    elif final in "hl" and private:
        for item in values:
            if item == 1049 and final == "h":
                screen.clear()


def render(data: str, rows: int, cols: int) -> str:
    screen = Screen(rows, cols)
    index = 0
    length = len(data)
    while index < length:
        ch = data[index]
        if ch == "\x1b":
            if index + 1 >= length:
                break
            nxt = data[index + 1]
            if nxt == "[":
                end = index + 2
                while end < length and not ("\x40" <= data[end] <= "\x7e"):
                    end += 1
                if end >= length:
                    break
                apply_csi(screen, data[index + 2 : end], data[end])
                index = end + 1
                continue
            if nxt == "]":
                end = index + 2
                while end < length and data[end] not in ("\x07",):
                    if data[end] == "\x1b" and end + 1 < length and data[end + 1] == "\\":
                        end += 1
                        break
                    end += 1
                index = end + 1
                continue
            if nxt in "()":
                index += 3
                continue
            index += 2
            continue
        if ch == "\r":
            screen.col = 0
        elif ch == "\n":
            screen.newline()
        elif ch == "\b":
            screen.col = max(0, screen.col - 1)
        elif ch == "\t":
            screen.col = min(screen.cols - 1, (screen.col // 8 + 1) * 8)
        elif ch >= " ":
            screen.put(ch)
        index += 1
    return screen.render()


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: ansi-screen.py <transcript> [rows] [cols]", file=sys.stderr)
        return 2
    path = sys.argv[1]
    rows = int(sys.argv[2]) if len(sys.argv) > 2 else 50
    cols = int(sys.argv[3]) if len(sys.argv) > 3 else 120
    with open(path, "r", encoding="utf-8", errors="replace") as handle:
        data = handle.read()
    print(render(data, rows, cols))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
