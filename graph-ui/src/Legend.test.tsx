import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { Legend } from "./Legend";

afterEach(() => cleanup());

describe("Legend", () => {
  it("is collapsed by default", () => {
    render(<Legend />);
    expect(screen.getByRole("button", { name: "Show legend" })).toBeInTheDocument();
    expect(screen.queryByText("Node kinds")).not.toBeInTheDocument();
  });

  it("opens and closes on toggle", () => {
    render(<Legend />);
    fireEvent.click(screen.getByRole("button", { name: "Show legend" }));
    expect(screen.getByText("Node kinds")).toBeInTheDocument();
    expect(screen.getByText("Relationships")).toBeInTheDocument();
    expect(screen.getByText("Stars — degree")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Hide legend" }));
    expect(screen.queryByText("Node kinds")).not.toBeInTheDocument();
  });
});
