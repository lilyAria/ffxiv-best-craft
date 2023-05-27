use ffxiv_crafting::{Actions, Attributes, Recipe, Status};

#[derive(Hash, Eq, PartialEq, Clone)]
pub struct SolverHash {
    pub attributes: Attributes,
    pub recipe: Recipe,
}

pub trait Solver {
    fn init(&mut self);
    fn read(&self, s: &Status) -> Option<Actions>;
    fn read_all(&self, s: &Status) -> Vec<Actions> {
        let mut result = Vec::new();
        let mut status = s.clone();
        while let Some(action) = self.read(&status) {
            if status.is_action_allowed(action).is_err() {
                break;
            }
            status.cast_action(action);
            result.push(action);
            if status.is_finished() {
                break;
            }
        }
        result
    }
}
