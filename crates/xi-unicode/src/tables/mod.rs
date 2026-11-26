// Copyright 2016 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Raw trie data for linebreak property lookup.

mod lb1_2;
mod lb3;
mod lb4;
mod state_machine;

pub use self::lb1_2::LINEBREAK_1_2;
pub use self::lb3::LINEBREAK_3_CHILD;
pub use self::lb3::LINEBREAK_3_ROOT;
pub use self::lb4::LINEBREAK_4_LEAVES;
pub use self::lb4::LINEBREAK_4_MID;
pub use self::lb4::LINEBREAK_4_ROOT;
pub use self::state_machine::LINEBREAK_STATE_MACHINE;
pub use self::state_machine::LINEBREAK_STATE_MAP;
pub use self::state_machine::N_LINEBREAK_CATEGORIES;
