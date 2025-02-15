package main

import (
	"flag"
	"fmt"
	"log"
	"os"
	"regexp"
	"strings"
)

func main() {
	flag.Parse()

	outputFile := "src/gui/zui/stubs.zig"
	log.Printf("generating zui stubs to %s\n", outputFile)
	defer log.Println("stub generation done")

	buf, err := os.ReadFile("src/gui/zui/zui.zig")
	if err != nil {
		panic(err)
	}

	output := `//! @NOTE (jrc): This code is auto-generated by scripts/generate_zui_stubs/main.go and will be
//! automatically overwritten the next time the script is run. DO NOT MANUALLY EDIT!

const std = @import("std");

const zui = @import("../zui.zig");
const cimgui = @import("cimgui");
const imgui = cimgui.c;

`
	// replace param names with underscores to avoid unused variable errors
	re := regexp.MustCompile(`[a-zA-Z0-9_]+:`)

	for _, line := range strings.Split(string(buf), "\n") {
		if !strings.HasPrefix(line, "pub fn ") {
			continue
		}

		line = re.ReplaceAllString(line, "_:")

		parts := strings.Split(line, " ")
		returnType := parts[len(parts)-2]
		val := ""

		switch returnType {
		case "void":
		case "bool":
			val = "return true;"
		case "u8", "u32", "f32", "zui.ID":
			val = "return 0;"
		case "zui.ImVec2":
			val = "return .{};"
		case "*zui.Style":
			line = strings.Replace(line, "pub fn", "pub inline fn", -1)
			val = "var val = std.mem.zeroes(zui.Style); return &val;"
		case "*imgui.ImGuiViewport":
			val = "var val = std.mem.zeroes(imgui.ImGuiViewport); return &val;"
		case "?*imgui.ImGuiDockNode":
			val = "return null;"
		default:
			panic("unimplemented zui return type: " + returnType)
		}

		// special-cases: things that should always return false in headless mode
		falses := []string{
			"selectable",
			"button",
			"isMouseClicked",
		}
		for _, item := range falses {
			prefix := fmt.Sprintf("pub fn %s(", item)
			if strings.HasPrefix(line, prefix) {
				val = "return false;"
			}
		}

		output += fmt.Sprintf("%s %s }\n\n", line, val)
	}

	os.WriteFile(outputFile, []byte(output), 0o666)
}
