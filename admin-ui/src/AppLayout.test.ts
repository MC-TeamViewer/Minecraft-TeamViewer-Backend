import source from "./App.vue?raw";

describe("admin functional tabs", () => {
  it("defaults to overview and declares the five approved tab groups", () => {
    expect(source).toContain('const activeTab = ref("overview")');
    expect(source.match(/<el-tab-pane /g)).toHaveLength(5);
    for (const [label, name] of [
      ["概览", "overview"],
      ["流量", "traffic"],
      ["活跃指标", "metrics"],
      ["数据管理", "data"],
      ["审计日志", "audit"],
    ]) {
      expect(source).toContain(`label="${label}" name="${name}"`);
    }
  });

  it("lazy-loads non-overview panes and keeps cleanup in data management", () => {
    expect(source.match(/<el-tab-pane[^>]+lazy>/g)).toHaveLength(4);
    const dataPane = source.slice(source.indexOf('label="数据管理"'), source.indexOf('label="审计日志"'));
    expect(dataPane).toContain("<RoomDataCleanup");
    expect(dataPane).toContain("<HistoryDataManager");
    expect(dataPane).toContain("<RuntimeStateExplorer");
  });
});
