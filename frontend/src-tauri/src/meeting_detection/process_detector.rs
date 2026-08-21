use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

const NEW_TEAMS_PROCESS_NAME: &str = "ms-teams.exe";

#[derive(Debug, Clone, Default)]
pub struct TeamsProcessSnapshot {
    pub running: bool,
    pub process_ids: Vec<u32>,
}

pub struct TeamsProcessDetector {
    system: System,
}

impl TeamsProcessDetector {
    pub fn new() -> Self {
        Self {
            system: System::new(),
        }
    }

    pub fn detect(&mut self) -> TeamsProcessSnapshot {
        // Names come from the process enumeration itself, so nothing beyond the
        // empty refresh kind is needed — and skipping `exe` in particular avoids
        // opening a handle to every process on the machine every 15 seconds.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::new(),
        );

        detect_teams_process_from_names(
            self.system
                .processes()
                .iter()
                .filter_map(|(pid, process)| Some((pid.as_u32(), process.name().to_str()?))),
        )
    }
}

pub fn detect_teams_process() -> TeamsProcessSnapshot {
    TeamsProcessDetector::new().detect()
}

pub fn detect_teams_process_from_names<I, S>(processes: I) -> TeamsProcessSnapshot
where
    I: IntoIterator<Item = (u32, S)>,
    S: AsRef<str>,
{
    let process_ids: Vec<u32> = processes
        .into_iter()
        .filter_map(|(pid, name)| {
            if name.as_ref().eq_ignore_ascii_case(NEW_TEAMS_PROCESS_NAME) {
                Some(pid)
            } else {
                None
            }
        })
        .collect();

    TeamsProcessSnapshot {
        running: !process_ids.is_empty(),
        process_ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_new_teams_case_insensitively() {
        let snapshot = detect_teams_process_from_names([
            (1, "explorer.exe"),
            (2, "MS-TEAMS.EXE"),
            (3, "msedgewebview2.exe"),
        ]);

        assert!(snapshot.running);
        assert_eq!(snapshot.process_ids, vec![2]);
    }

    #[test]
    fn ignores_classic_teams_for_mvp() {
        let snapshot = detect_teams_process_from_names([(1, "Teams.exe")]);

        assert!(!snapshot.running);
        assert!(snapshot.process_ids.is_empty());
    }
}
