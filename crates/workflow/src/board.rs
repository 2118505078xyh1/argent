//! Shared task-board prototype for agent teams.
//!
//! This is intentionally small: a workflow becomes a board, agents can claim
//! ready tasks and complete them, and dependents become ready when their
//! prerequisites are done. Hosts may wrap it in a `Mutex`/`RwLock`; persistence
//! and UI are deliberately out of scope for the prototype.

use std::collections::BTreeMap;

use argent_core::error::{Error, Result};

use crate::Workflow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardStatus {
    Open,
    Claimed,
    Done,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardTask {
    pub id: String,
    pub title: String,
    pub role: String,
    pub depends_on: Vec<String>,
    pub status: BoardStatus,
    pub owner: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Board {
    tasks: BTreeMap<String, BoardTask>,
}

impl Board {
    pub fn new() -> Board {
        Board::default()
    }

    /// Build one task per workflow step, preserving dependencies.
    pub fn from_workflow(workflow: &Workflow) -> Board {
        let mut board = Board::new();
        for step in &workflow.steps {
            board.tasks.insert(
                step.id.clone(),
                BoardTask {
                    id: step.id.clone(),
                    title: step.task.clone(),
                    role: step.role.clone(),
                    depends_on: step.depends_on.clone(),
                    status: BoardStatus::Open,
                    owner: None,
                    note: None,
                },
            );
        }
        board
    }

    pub fn tasks(&self) -> Vec<&BoardTask> {
        self.tasks.values().collect()
    }

    pub fn get(&self, id: &str) -> Option<&BoardTask> {
        self.tasks.get(id)
    }

    /// Tasks whose dependencies are all done and which nobody has claimed.
    pub fn ready(&self) -> Vec<&BoardTask> {
        self.tasks
            .values()
            .filter(|task| {
                task.status == BoardStatus::Open
                    && task.depends_on.iter().all(|dependency| {
                        self.tasks
                            .get(dependency)
                            .is_some_and(|task| task.status == BoardStatus::Done)
                    })
            })
            .collect()
    }

    pub fn claim(&mut self, id: &str, owner: &str) -> Result<()> {
        if self.get(id).is_none() {
            return Err(Error::config(format!("no board task '{}'", id)));
        }
        if !self.dependencies_done(id) {
            return Err(Error::config(format!(
                "task '{}' has unfinished dependencies",
                id
            )));
        }
        let task = self.task_mut(id)?;
        match task.status {
            BoardStatus::Open => {}
            BoardStatus::Claimed => {
                return Err(Error::config(format!(
                    "task '{}' is already claimed by {}",
                    id,
                    task.owner.as_deref().unwrap_or("someone")
                )))
            }
            BoardStatus::Done => {
                return Err(Error::config(format!("task '{}' is already done", id)))
            }
            BoardStatus::Blocked => return Err(Error::config(format!("task '{}' is blocked", id))),
        }
        task.status = BoardStatus::Claimed;
        task.owner = Some(owner.to_string());
        Ok(())
    }

    pub fn complete(&mut self, id: &str, note: impl Into<String>) -> Result<()> {
        let task = self.task_mut(id)?;
        if task.status == BoardStatus::Done {
            return Ok(());
        }
        task.status = BoardStatus::Done;
        task.note = Some(note.into());
        Ok(())
    }

    pub fn release(&mut self, id: &str) -> Result<()> {
        let task = self.task_mut(id)?;
        if task.status == BoardStatus::Done {
            return Err(Error::config(format!("task '{}' is already done", id)));
        }
        task.status = BoardStatus::Open;
        task.owner = None;
        Ok(())
    }

    pub fn block(&mut self, id: &str, note: impl Into<String>) -> Result<()> {
        let task = self.task_mut(id)?;
        task.status = BoardStatus::Blocked;
        task.note = Some(note.into());
        Ok(())
    }

    fn dependencies_done(&self, id: &str) -> bool {
        self.tasks
            .get(id)
            .map(|task| {
                task.depends_on.iter().all(|dependency| {
                    self.tasks
                        .get(dependency)
                        .is_some_and(|task| task.status == BoardStatus::Done)
                })
            })
            .unwrap_or(false)
    }

    fn task_mut(&mut self, id: &str) -> Result<&mut BoardTask> {
        self.tasks
            .get_mut(id)
            .ok_or_else(|| Error::config(format!("no board task '{}'", id)))
    }

    /// Snapshot of ids by status, for a status panel.
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for task in self.tasks.values() {
            let key = match task.status {
                BoardStatus::Open => "open",
                BoardStatus::Claimed => "claimed",
                BoardStatus::Done => "done",
                BoardStatus::Blocked => "blocked",
            };
            *counts.entry(key).or_insert(0) += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Step, Workflow};
    use std::time::Duration;

    fn workflow() -> Workflow {
        Workflow {
            name: "review".to_string(),
            steps: vec![
                Step {
                    id: "scan".to_string(),
                    role: "scout".to_string(),
                    task: "find entry points".to_string(),
                    depends_on: Vec::new(),
                    model: None,
                    thinking: None,
                },
                Step {
                    id: "review".to_string(),
                    role: "reviewer".to_string(),
                    task: "review the diff".to_string(),
                    depends_on: vec!["scan".to_string()],
                    model: None,
                    thinking: None,
                },
            ],
            max_concurrency: 2,
            timeout: Duration::from_secs(30),
        }
    }

    #[test]
    fn a_workflow_becomes_a_dependency_aware_board() {
        let mut board = Board::from_workflow(&workflow());
        assert_eq!(board.ready().len(), 1);
        assert_eq!(board.ready()[0].id, "scan");
        board.claim("scan", "agent-a").unwrap();
        assert_eq!(board.ready().len(), 0);
        board.complete("scan", "done").unwrap();
        assert_eq!(board.ready()[0].id, "review");
        board.claim("review", "agent-b").unwrap();
        board.complete("review", "looks good").unwrap();
        assert_eq!(board.counts()["done"], 2);
    }

    #[test]
    fn a_claimed_task_cannot_be_claimed_twice() {
        let mut board = Board::from_workflow(&workflow());
        board.claim("scan", "agent-a").unwrap();
        assert!(board.claim("scan", "agent-b").is_err());
        board.release("scan").unwrap();
        board.claim("scan", "agent-b").unwrap();
    }

    #[test]
    fn blocked_tasks_are_not_ready() {
        let mut board = Board::from_workflow(&workflow());
        board.block("scan", "waiting on credentials").unwrap();
        assert!(board.ready().is_empty());
        assert!(board.claim("scan", "agent-a").is_err());
    }
}
