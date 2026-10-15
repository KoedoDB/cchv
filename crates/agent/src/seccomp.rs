//! The seccomp profile of every container.
//!
//! The OCI spec the host sends carries a seccomp profile only when the pod asks for one (`RuntimeDefault`, `Localhost`); Kubernetes'
//! default is Unconfined. In the VM a container is root of the guest, with the guest's kernel shared with the agent, so the guest
//! kernel's attack surface is cut for every container, not just the ones that ask:
//!
//! - **No profile in the spec**: the profile below is installed. Everything is allowed except what is listed, which answers
//!   `EPERM` (and `clone3`, which answers `ENOSYS`, so that libc falls back to `clone`). It follows the idea of the containerd/Docker
//!   default profile, as a deny list: kernel modules, `kexec`, `bpf`, `perf_event_open`, `userfaultfd`, `io_uring`, the key ring, the
//!   mount family, namespaces (`unshare`, `setns`, `clone` with a `CLONE_NEW*` flag), `process_vm_*`, handles, clocks, I/O ports, NUMA
//!   policy, `personality` with the dangerous bits, and `socket()` of the families a container has no use for: above all `AF_VSOCK`,
//!   the way to the host (and the agent's own listener).
//! - **A profile in the spec**: it is kept, and these denials are put on top of it (see `enforce`). There is no way for the pod to
//!   switch this off.
//!
//! Some of these need a capability the container does not have anyway; they are listed regardless, so that the profile does not
//! depend on the capability list staying as it is.

use serde_json::{json, Map, Value};

/// `EPERM`, and `ENOSYS`.
const EPERM: i64 = 1;
const ENOSYS: i64 = 38;

/// Syscalls that answer `EPERM`, whatever the arguments.
pub const DENIED: &[&str] = &[
    // kernel code and its state
    "kexec_load",
    "kexec_file_load",
    "init_module",
    "finit_module",
    "delete_module",
    "create_module",
    "get_kernel_syms",
    "query_module",
    "bpf",
    "perf_event_open",
    "userfaultfd",
    "io_uring_setup",
    "io_uring_enter",
    "io_uring_register",
    "syslog",
    "_sysctl",
    "sysfs",
    "uselib",
    "ustat",
    "nfsservctl",
    "lookup_dcookie",
    "acct",
    "quotactl",
    "quotactl_fd",
    "kcmp",
    "fanotify_init",
    "vhangup",
    "reboot",
    "swapon",
    "swapoff",
    // the key ring
    "keyctl",
    "add_key",
    "request_key",
    // mounts and namespaces
    "mount",
    "umount",
    "umount2",
    "pivot_root",
    "mount_setattr",
    "move_mount",
    "open_tree",
    "fsopen",
    "fsconfig",
    "fsmount",
    "fspick",
    "unshare",
    "setns",
    "sethostname",
    "setdomainname",
    // other processes' memory, kernel object handles
    "process_vm_readv",
    "process_vm_writev",
    "open_by_handle_at",
    "name_to_handle_at",
    // the clock
    "settimeofday",
    "clock_settime",
    "clock_adjtime",
    "adjtimex",
    // hardware
    "iopl",
    "ioperm",
    "vm86",
    "vm86old",
    "modify_ldt",
    "pkey_alloc",
    "pkey_free",
    "pkey_mprotect",
    // NUMA policy
    "move_pages",
    "mbind",
    "set_mempolicy",
    "migrate_pages",
];

/// `socket()` families a container may use: AF_UNSPEC (0), AF_UNIX (1), AF_INET (2), AF_INET6 (10), AF_NETLINK (16). Every other family
/// is refused: the rest are rarely used by workloads and have been a steady source of kernel bugs (AF_RDS, AF_TIPC, AF_SMC, AF_X25,
/// AF_APPLETALK, AF_NFC, AF_CAN, AF_BLUETOOTH, AF_KEY, AF_ALG, AF_PACKET, ...), and AF_VSOCK (40) is the way to the host and to the
/// agent's own listener. (Not all of them are in the guest kernel; the list does not depend on it.)
pub const ALLOWED_SOCKET_FAMILIES: &[u64] = &[0, 1, 2, 10, 16];
/// Family numbers up to the kernel's AF_MAX (46); a number above is no family.
const AF_MAX: u64 = 46;

pub fn denied_socket_families() -> Vec<u64> {
    (0..=AF_MAX).filter(|f| !ALLOWED_SOCKET_FAMILIES.contains(f)).collect()
}

/// The `CLONE_NEW*` flags: CLONE_NEWNS, CLONE_NEWCGROUP, CLONE_NEWUTS, CLONE_NEWIPC, CLONE_NEWUSER, CLONE_NEWPID, CLONE_NEWNET,
/// and CLONE_NEWTIME (which `clone` cannot take, `unshare` can; listed for completeness).
const CLONE_NEW_FLAGS: &[u64] = &[
    0x0002_0000,
    0x0200_0000,
    0x0400_0000,
    0x0800_0000,
    0x1000_0000,
    0x2000_0000,
    0x4000_0000,
];

/// `personality` values with one of these bits are refused: MMAP_PAGE_ZERO, ADDR_COMPAT_LAYOUT, READ_IMPLIES_EXEC, ADDR_LIMIT_32BIT.
/// They weaken the address space for an attacker's benefit; nothing a container runs needs them. (The query `personality(0xffffffff)` has
/// every bit set and is refused with them: a rule can have one condition on an argument, so "except 0xffffffff" cannot be said, and
/// libseccomp does not AND two conditions on the same argument. Nothing a container runs asks.)
const PERSONALITY_BITS: &[u64] = &[0x0010_0000, 0x0020_0000, 0x0040_0000, 0x0080_0000];

#[cfg(target_arch = "x86_64")]
fn architectures() -> Value {
    json!(["SCMP_ARCH_X86_64", "SCMP_ARCH_X86", "SCMP_ARCH_X32"])
}
#[cfg(target_arch = "aarch64")]
fn architectures() -> Value {
    json!(["SCMP_ARCH_AARCH64", "SCMP_ARCH_ARM"])
}
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn architectures() -> Value {
    json!([])
}

fn arg(index: u32, value: u64, value_two: u64, op: &str) -> Value {
    json!({"index": index, "value": value, "valueTwo": value_two, "op": op})
}

fn rule(names: &[&str], errno: i64, args: Vec<Value>) -> Value {
    let mut m = Map::new();
    m.insert("names".to_string(), json!(names));
    m.insert("action".to_string(), json!("SCMP_ACT_ERRNO"));
    m.insert("errnoRet".to_string(), json!(errno));
    if !args.is_empty() {
        m.insert("args".to_string(), Value::Array(args));
    }
    Value::Object(m)
}

/// The denials, as `syscalls` entries of a profile whose default action allows.
pub fn denial_rules() -> Vec<Value> {
    let mut rules = vec![
        rule(DENIED, EPERM, vec![]),
        // clone3 takes its flags in a structure seccomp cannot read: it is refused as if the kernel did not have it, and libc uses clone.
        rule(&["clone3"], ENOSYS, vec![]),
    ];
    // socket(domain, ...): domain is argument 0.
    // One rule for each refused family: libseccomp does not AND several conditions on one argument.
    for family in denied_socket_families() {
        rules.push(rule(&["socket"], EPERM, vec![arg(0, family, 0, "SCMP_CMP_EQ")]));
    }
    // clone(flags, ...): a namespace flag is refused. (arg & flag) == flag.
    for flag in CLONE_NEW_FLAGS {
        rules.push(rule(&["clone"], EPERM, vec![arg(0, *flag, *flag, "SCMP_CMP_MASKED_EQ")]));
    }
    // personality(persona): a dangerous bit set. One condition per rule.
    for bit in PERSONALITY_BITS {
        rules.push(rule(&["personality"], EPERM, vec![arg(0, *bit, *bit, "SCMP_CMP_MASKED_EQ")]));
    }
    rules
}

/// The profile for a container whose spec has none.
pub fn default_profile() -> Value {
    json!({
        "defaultAction": "SCMP_ACT_ALLOW",
        "architectures": architectures(),
        "syscalls": denial_rules(),
    })
}

/// Whether the condition (a seccomp argument comparison) is true for the value `v`; None for an operator not known.
fn condition_holds(c: &Value, v: u64) -> Option<bool> {
    let value = c["value"].as_u64()?;
    let two = c["valueTwo"].as_u64().unwrap_or(0);
    Some(match c["op"].as_str()? {
        "SCMP_CMP_EQ" => v == value,
        "SCMP_CMP_NE" => v != value,
        "SCMP_CMP_LT" => v < value,
        "SCMP_CMP_LE" => v <= value,
        "SCMP_CMP_GT" => v > value,
        "SCMP_CMP_GE" => v >= value,
        "SCMP_CMP_MASKED_EQ" => v & value == two,
        _ => return None,
    })
}

fn allows(action: Option<&str>) -> bool {
    matches!(action, Some("SCMP_ACT_ALLOW") | Some("SCMP_ACT_LOG") | Some("SCMP_ACT_TRACE") | Some("SCMP_ACT_NOTIFY"))
}

/// The profile to give crun for a spec whose seccomp section is `existing`.
///
/// - None: `default_profile()`.
/// - A profile that allows by default: the denials are put first.
/// - A profile that refuses by default (an allow list, like `RuntimeDefault`): what it allows of the denied syscalls is taken out of
///   it, so that they stay refused, and the allow of `socket` is narrowed to the families that are not refused. (Overlapping rules for
///   one syscall are not relied on: libseccomp does not promise which one wins.) A `clone` allowed without conditions would still allow
///   namespace flags: the containerd profile does not do that, and a profile from outside is not something a pod can bring.
///
/// The listener fields are removed in both cases: crun would connect, as root in the guest, to a socket the spec names.
pub fn enforce(existing: Option<&Value>) -> Value {
    let Some(Value::Object(profile)) = existing else {
        return default_profile();
    };
    let mut profile = profile.clone();
    profile.remove("listenerPath");
    profile.remove("listenerMetadata");

    let default_allows = allows(profile.get("defaultAction").and_then(|a| a.as_str()));
    let mut kept: Vec<Value> = Vec::new();
    let mut socket_families_allowed = std::collections::HashSet::new();

    for entry in profile.get("syscalls").and_then(|s| s.as_array()).cloned().unwrap_or_default() {
        let Value::Object(mut e) = entry else { continue };
        if !default_allows && allows(e.get("action").and_then(|a| a.as_str())) {
            let names: Vec<Value> = e
                .get("names")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|n| n.as_str().map(|n| !DENIED.contains(&n) && n != "clone3").unwrap_or(false))
                .collect();
            if names.is_empty() {
                continue;
            }
            // socket: allowed only for the allowed families, one rule each (libseccomp does not AND several conditions on one
            // argument, so "any family but these" cannot be written as one rule). What an entry allows of socket is worked out
            // as a set of families and cut to the allowed ones: no conditions = every family, `domain == f` = {f}, `domain != f` =
            // every family but f; any other condition on it allows nothing (closed).
            if names.iter().any(|n| n.as_str() == Some("socket")) {
                let others: Vec<Value> = names.iter().filter(|n| n.as_str() != Some("socket")).cloned().collect();
                let args = e.get("args").and_then(|a| a.as_array()).cloned().unwrap_or_default();
                let action = e.get("action").cloned().unwrap_or(json!("SCMP_ACT_ALLOW"));
                if !others.is_empty() {
                    let mut rest = e.clone();
                    rest.insert("names".to_string(), Value::Array(others));
                    kept.push(Value::Object(rest));
                }
                let families: Vec<u64> = if args.is_empty() {
                    ALLOWED_SOCKET_FAMILIES.to_vec()
                } else if args.len() == 1 && args[0]["index"] == 0 {
                    ALLOWED_SOCKET_FAMILIES
                        .iter()
                        .copied()
                        .filter(|f| condition_holds(&args[0], *f) == Some(true))
                        .collect()
                } else {
                    vec![]
                };
                for family in families {
                    if socket_families_allowed.insert(family) {
                        kept.push(json!({"names": ["socket"], "action": action, "args": [arg(0, family, 0, "SCMP_CMP_EQ")]}));
                    }
                }
                continue;
            }
            e.insert("names".to_string(), Value::Array(names));
        }
        kept.push(Value::Object(e));
    }

    let mut syscalls = if default_allows { denial_rules() } else { Vec::new() };
    syscalls.extend(kept);
    profile.insert("syscalls".to_string(), Value::Array(syscalls));
    if !profile.contains_key("architectures") {
        profile.insert("architectures".to_string(), architectures());
    }
    Value::Object(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_of(rule: &Value) -> Vec<String> {
        rule["names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_string())
            .collect()
    }

    fn find<'a>(profile: &'a Value, name: &str) -> Vec<&'a Value> {
        profile["syscalls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| names_of(r).iter().any(|n| n == name))
            .collect()
    }

    #[test]
    fn a_spec_without_seccomp_gets_the_default_profile() {
        let p = enforce(None);
        assert_eq!(p, default_profile());
        assert_eq!(p["defaultAction"], "SCMP_ACT_ALLOW");
        assert!(!p["architectures"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_dangerous_syscalls_answer_eperm() {
        let p = default_profile();
        for name in ["bpf", "perf_event_open", "userfaultfd", "unshare", "setns", "mount", "keyctl", "init_module", "kexec_load"] {
            let r = find(&p, name);
            assert_eq!(r.len(), 1, "{name}");
            assert_eq!(r[0]["action"], "SCMP_ACT_ERRNO");
            assert_eq!(r[0]["errnoRet"], 1);
            assert!(r[0].get("args").is_none(), "{name} is refused whatever the arguments");
        }
    }

    #[test]
    fn clone3_answers_enosys_so_libc_falls_back_to_clone() {
        let p = default_profile();
        let r = find(&p, "clone3");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["errnoRet"], 38);
    }

    #[test]
    fn vsock_and_the_other_unneeded_socket_families_are_refused() {
        let p = default_profile();
        let r = find(&p, "socket");
        let refused: Vec<u64> = r.iter().map(|r| r["args"][0]["value"].as_u64().unwrap()).collect();
        for family in [40u64, 17, 38, 15, 44, 21, 30, 43] {
            assert!(refused.contains(&family), "family {family} must be refused");
        }
        for rule in &r {
            assert_eq!(rule["action"], "SCMP_ACT_ERRNO");
            assert_eq!(rule["args"][0]["index"], 0);
            assert_eq!(rule["args"][0]["op"], "SCMP_CMP_EQ");
        }
        // what workloads use stays allowed: AF_UNSPEC, AF_UNIX, AF_INET, AF_INET6, AF_NETLINK
        for family in ALLOWED_SOCKET_FAMILIES {
            assert!(!refused.contains(family), "family {family} must stay allowed");
        }
    }

    #[test]
    fn no_rule_has_two_conditions_on_one_argument() {
        // libseccomp does not AND them (a rule "domain != 15 and domain != 40" allowed 40): every rule has at most one per index.
        let mut profiles = vec![default_profile(), enforce(Some(&runtime_default_like()))];
        profiles.push(enforce(Some(&json!({
            "defaultAction": "SCMP_ACT_ALLOW",
            "syscalls": [{"names": ["socket"], "action": "SCMP_ACT_ERRNO", "args": [{"index": 0, "value": 1, "op": "SCMP_CMP_EQ"}]}]
        }))));
        for p in profiles {
            for r in p["syscalls"].as_array().unwrap() {
                let mut seen = std::collections::HashSet::new();
                for a in r["args"].as_array().into_iter().flatten() {
                    assert!(seen.insert(a["index"].as_u64().unwrap()), "two conditions on one argument in {r}");
                }
            }
        }
    }

    #[test]
    fn namespace_flags_of_clone_are_refused_one_by_one() {
        let p = default_profile();
        let r = find(&p, "clone");
        assert_eq!(r.len(), CLONE_NEW_FLAGS.len());
        for (rule, flag) in r.iter().zip(CLONE_NEW_FLAGS) {
            assert_eq!(rule["args"][0]["op"], "SCMP_CMP_MASKED_EQ");
            assert_eq!(rule["args"][0]["value"], *flag);
            assert_eq!(rule["args"][0]["valueTwo"], *flag, "refused when the whole flag is set");
        }
    }

    #[test]
    fn personality_is_refused_only_with_the_dangerous_bits() {
        let p = default_profile();
        let r = find(&p, "personality");
        assert_eq!(r.len(), PERSONALITY_BITS.len());
        for rule in r {
            assert_eq!(rule["args"].as_array().unwrap().len(), 1);
            assert_eq!(rule["args"][0]["op"], "SCMP_CMP_MASKED_EQ");
        }
    }

    #[test]
    fn every_rule_is_in_the_oci_format() {
        let p = default_profile();
        for r in p["syscalls"].as_array().unwrap() {
            assert!(r["names"].as_array().is_some_and(|n| !n.is_empty()));
            assert!(r["action"].as_str().unwrap().starts_with("SCMP_ACT_"));
            for a in r["args"].as_array().into_iter().flatten() {
                assert!(a["index"].as_u64().unwrap() < 6);
                assert!(a["value"].is_u64() && a["valueTwo"].is_u64());
                assert!(a["op"].as_str().unwrap().starts_with("SCMP_CMP_"));
            }
        }
        // it is JSON that survives being written and read back
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), p);
        // no name twice among the unconditional rules
        let mut seen = std::collections::HashSet::new();
        for n in DENIED {
            assert!(seen.insert(*n), "{n} is listed twice");
        }
    }

    fn runtime_default_like() -> Value {
        json!({
            "defaultAction": "SCMP_ACT_ERRNO",
            "defaultErrnoRet": 1,
            "listenerPath": "/run/x.sock",
            "listenerMetadata": "m",
            "architectures": ["SCMP_ARCH_X86_64"],
            "syscalls": [
                {"names": ["read", "write", "socket", "bpf", "mount", "clone3", "futex"], "action": "SCMP_ACT_ALLOW"},
                {"names": ["clone"], "action": "SCMP_ACT_ALLOW",
                 "args": [{"index": 0, "value": 2114060288u64, "valueTwo": 0, "op": "SCMP_CMP_MASKED_EQ"}]},
                {"names": ["mount", "unshare"], "action": "SCMP_ACT_ALLOW"}
            ]
        })
    }

    #[test]
    fn an_allow_list_keeps_what_it_allows_except_the_denied_syscalls() {
        let p = enforce(Some(&runtime_default_like()));
        assert_eq!(p["defaultAction"], "SCMP_ACT_ERRNO");
        assert!(p.get("listenerPath").is_none() && p.get("listenerMetadata").is_none());
        for name in ["bpf", "mount", "unshare", "clone3"] {
            assert!(find(&p, name).is_empty(), "{name} must not be allowed any more");
        }
        for name in ["read", "write", "futex"] {
            assert_eq!(find(&p, name).len(), 1, "{name} stays allowed");
        }
        // the conditional clone of the profile is untouched
        assert_eq!(find(&p, "clone").len(), 1);
    }

    #[test]
    fn an_allow_list_allows_socket_only_for_the_allowed_families_one_rule_each() {
        let p = enforce(Some(&runtime_default_like()));
        let s = find(&p, "socket");
        assert_eq!(s.len(), ALLOWED_SOCKET_FAMILIES.len());
        let mut families: Vec<u64> = s.iter().map(|r| r["args"][0]["value"].as_u64().unwrap()).collect();
        families.sort();
        assert_eq!(families, ALLOWED_SOCKET_FAMILIES.to_vec());
        for r in s {
            assert_eq!(r["action"], "SCMP_ACT_ALLOW");
            assert_eq!(r["args"][0]["op"], "SCMP_CMP_EQ");
        }
        // and read/write do not share a rule with socket any more
        assert!(find(&p, "read").iter().all(|r| !names_of(r).contains(&"socket".to_string())));
    }

    #[test]
    fn an_allow_list_with_socket_conditions_keeps_only_the_allowed_families() {
        let own = json!({
            "defaultAction": "SCMP_ACT_ERRNO",
            "syscalls": [
                {"names": ["socket"], "action": "SCMP_ACT_ALLOW", "args": [{"index": 0, "value": 2, "op": "SCMP_CMP_EQ"}]},
                {"names": ["socket"], "action": "SCMP_ACT_ALLOW", "args": [{"index": 0, "value": 40, "op": "SCMP_CMP_EQ"}]},
                {"names": ["socket"], "action": "SCMP_ACT_ALLOW", "args": [{"index": 0, "value": 40, "op": "SCMP_CMP_NE"}]},
                {"names": ["socket", "read"], "action": "SCMP_ACT_ALLOW", "args": [{"index": 0, "value": 1, "op": "SCMP_CMP_EQ"}]}
            ]
        });
        let p = enforce(Some(&own));
        let families: Vec<u64> = find(&p, "socket").iter().map(|r| r["args"][0]["value"].as_u64().unwrap()).collect();
        // 2 and 1 from the equal rules; 40 refused; "!= 40" is every allowed family, of which 0, 10 and 16 are new (2 and 1 are not repeated)
        assert_eq!(families, vec![2, 0, 1, 10, 16]);
    }

    #[test]
    fn a_profile_that_allows_by_default_gets_the_denials_first() {
        let own = json!({
            "defaultAction": "SCMP_ACT_ALLOW",
            "syscalls": [{"names": ["chmod"], "action": "SCMP_ACT_ERRNO", "errnoRet": 1}]
        });
        let p = enforce(Some(&own));
        let list = p["syscalls"].as_array().unwrap();
        assert_eq!(list.last().unwrap()["names"][0], "chmod", "the profile's own rules come after ours");
        assert!(names_of(&list[0]).contains(&"bpf".to_string()));
        assert!(!find(&p, "socket").is_empty());
        assert!(p.get("architectures").is_some());
    }

    #[test]
    fn a_profile_with_a_deny_default_and_no_syscalls_stays_closed() {
        let p = enforce(Some(&json!({"defaultAction": "SCMP_ACT_ERRNO", "syscalls": []})));
        assert_eq!(p["defaultAction"], "SCMP_ACT_ERRNO");
        assert!(p["syscalls"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_seccomp_value_that_is_not_an_object_is_replaced_by_the_default() {
        assert_eq!(enforce(Some(&json!("unconfined"))), default_profile());
        assert_eq!(enforce(Some(&Value::Null)), default_profile());
    }

    /// For trying the profiles by hand against a real runtime (docker, crun): `CLOUDHV_SECCOMP_DUMP=/some/dir cargo test -- --ignored dump`.
    #[test]
    #[ignore]
    fn dump_the_profiles_for_trying_them_by_hand() {
        let dir = std::env::var("CLOUDHV_SECCOMP_DUMP").expect("set CLOUDHV_SECCOMP_DUMP to a directory");
        let dir = std::path::Path::new(&dir);
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("default.json"), serde_json::to_string_pretty(&default_profile()).unwrap()).unwrap();
        std::fs::write(
            dir.join("merged.json"),
            serde_json::to_string_pretty(&enforce(Some(&runtime_default_like()))).unwrap(),
        )
        .unwrap();
        // A real allow list to merge into: CLOUDHV_SECCOMP_EXISTING=<profile in OCI form>.
        if let Ok(existing) = std::env::var("CLOUDHV_SECCOMP_EXISTING") {
            let existing: Value = serde_json::from_str(&std::fs::read_to_string(existing).unwrap()).unwrap();
            std::fs::write(
                dir.join("merged-real.json"),
                serde_json::to_string_pretty(&enforce(Some(&existing))).unwrap(),
            )
            .unwrap();
        }
    }
}
