use ffxiv_crafting::{Actions, Buffs, Status};
use micro_ndarray::Array;
use std::cell::Cell;

#[derive(Clone, Copy, Default, Debug)]
pub struct Slot {
    score: u32,
    steps: u16,
    pub action: Option<Actions>,
}

pub struct Solver {
    init_status: Status,
    mn: bool,
    wn: usize,
    obz: bool,
    touch_caches: Array<Cell<Option<Slot>>, 9>,
}

impl Solver {
    const MAX_INNER_QUIET: usize = 10;
    const MAX_INNOVATION: usize = 4;
    const MAX_MANIPULATION: usize = 8;
    const MAX_GREAT_STRIDES: usize = 3;
    const MAX_TOUCH_COMBO: usize = 2;
    // const MAX_VENERATION: usize = 4;
    const MAX_OBSERVE: usize = 1;
    const TOUCH_SKILLS: [(Actions, u16); 15] = [
        (Actions::BasicTouch, 10),
        (Actions::StandardTouch, 10),
        (Actions::AdvancedTouch, 10),
        (Actions::PrudentTouch, 5),
        (Actions::PreparatoryTouch, 20),
        (Actions::TrainedFinesse, 0),
        (Actions::GreatStrides, 0),
        (Actions::ByregotsBlessing, 10),
        (Actions::Observe, 0),
        (Actions::FocusedTouch, 10),
        (Actions::Manipulation, 0),
        (Actions::Innovation, 0),
        (Actions::WasteNot, 0),
        (Actions::WasteNotII, 0),
        (Actions::MastersMend, 0),
    ];

    pub fn new(init_status: Status, mn: bool, wn: usize, obz: bool) -> Self {
        Self {
            mn,
            wn,
            obz,
            touch_caches: Array::new([
                obz as usize * Self::MAX_OBSERVE + 1,
                Self::MAX_INNER_QUIET + 1,
                Self::MAX_INNOVATION + 1,
                Self::MAX_GREAT_STRIDES + 1,
                mn as usize * Self::MAX_MANIPULATION + 1,
                wn + 1,
                Self::MAX_TOUCH_COMBO + 1,
                init_status.recipe.durability as usize / 5 + 1,
                init_status.attributes.craft_points as usize + 1,
            ]),
            init_status,
        }
    }

    pub fn next_touch(&self, craft_points: i32, durability: u16, buffs: Buffs) -> Slot {
        let this_cell = &self.touch_caches[[
            buffs.observed as usize,
            buffs.inner_quiet as usize,
            buffs.innovation as usize,
            buffs.great_strides as usize,
            buffs.manipulation as usize,
            buffs.wast_not as usize,
            buffs.touch_combo_stage as usize,
            durability as usize / 5,
            craft_points as usize,
        ]];
        if let Some(slot) = this_cell.get() {
            return slot;
        }
        let mut best = Slot {
            score: 0,
            steps: 0,
            action: None,
        };
        let mut init_status = self.init_status.clone();
        init_status.craft_points = craft_points;
        init_status.durability = durability;
        init_status.buffs = buffs;
        for (action, consumed_du) in Self::TOUCH_SKILLS {
            if init_status.is_action_allowed(action).is_err()
                || durability < init_status.calc_durability(consumed_du)
                || init_status.success_rate(action) < 100
                || (matches!(action, Actions::Manipulation) && !self.mn)
                || (matches!(action, Actions::WasteNotII) && self.wn < 8)
                || (matches!(action, Actions::WasteNot) && self.wn < 4)
                || (matches!(action, Actions::Observe) && !self.obz)
                || (matches!(action, Actions::FocusedTouch) && init_status.buffs.observed == 0)
            {
                continue;
            }
            let mut s = init_status.clone();
            s.cast_action(action);
            let mut score = s.quality;
            let mut steps = 1;
            if let Slot {
                score: next_score,
                steps: next_steps,
                action: Some(_),
            } = self.next_touch(s.craft_points, s.durability, s.buffs)
            {
                score += next_score;
                steps += next_steps;
            }
            if score
                .cmp(&best.score)
                .then_with(|| steps.cmp(&best.steps).reverse())
                .is_gt()
            {
                best = Slot {
                    score,
                    steps,
                    action: Some(action),
                };
            }
        }
        this_cell.set(Some(best));
        best
    }
}
