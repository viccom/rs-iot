// frontend/src/lib/logSource.ts
// 日志来源分类（诊断页方案 A）：诊断排障多看"运行实例日志"，与系统/框架日志分流。
// 分界按 tracing target 前缀——实例运行日志全部出自 rs_iot::plugin::builtin::*（9 个
// 内置插件的采集/北向/变换实例），编排层（plugin::instance）、脚本引擎与热载
// （plugin::lua_core::*）、统计（stat_tracker）及 api/pipeline/db 等归系统侧。
// 治本方案（inst_id 结构化字段 + 按实例过滤）另行立项，届时以 fields.inst_id 精分。

export type LogSource = 'all' | 'instance' | 'system'

const INSTANCE_TARGET_PREFIX = 'rs_iot::plugin::builtin::'

/** target → instance/system（builtin 前缀精确匹配，近似前缀如 builtinx 不误判）。 */
export function classifyLogTarget(target: string): 'instance' | 'system' {
  return target.startsWith(INSTANCE_TARGET_PREFIX) ? 'instance' : 'system'
}

/** 过滤谓词：all 恒真，否则要求分类相等。 */
export function matchesLogSource(target: string, source: LogSource): boolean {
  return source === 'all' || classifyLogTarget(target) === source
}
