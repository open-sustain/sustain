// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Multinomial logistic regression over a fixed set of candidates, the
//! model the analyzer's learned decisions share.
//!
//! Each labelled example offers some candidates — the 24 keys of a track,
//! the four bar positions of a grid — each described by a feature vector,
//! and names the one that is right. A candidate's score is the weights'
//! dot product with its features, and the candidates' probabilities are
//! the softmax of their scores. The fit finds the weights that make the
//! right candidates most probable, with a ridge penalty on their squares.
//! The objective is convex and the ridge makes it strictly so, so Newton's
//! method finds its one optimum in a few steps, from any start.

/// The fit stops once a Newton step promises to lower the objective by
/// less than this (half the Newton decrement): far below any change to a
/// learned weight's printed digits, yet above the rounding of a sum over
/// thousands of examples.
const DECREASE_TOLERANCE: f64 = 1e-14;

/// More Newton steps than a fit this small ever needs.
const MAX_NEWTON_STEPS: usize = 100;

/// One labelled example: its candidates' features, and which is right.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// One feature vector per candidate, all of the fit's dimension.
    pub candidates: Vec<Vec<f64>>,
    /// The index of the right candidate.
    pub chosen: usize,
}

/// The index of the candidate `weights` score highest; ties go to the
/// earliest.
pub fn best(candidates: &[Vec<f64>], weights: &[f64]) -> usize {
    let scores: Vec<f64> = candidates.iter().map(|x| dot(x, weights)).collect();
    (0..scores.len()).fold(0, |best, index| {
        if scores[index] > scores[best] {
            index
        } else {
            best
        }
    })
}

/// The share of `choices` whose right candidate `weights` score highest.
pub fn accuracy(choices: &[Choice], weights: &[f64]) -> f64 {
    let right = choices
        .iter()
        .filter(|choice| best(&choice.candidates, weights) == choice.chosen)
        .count();
    right as f64 / choices.len().max(1) as f64
}

/// The weights, of `dimension` entries, minimizing the mean negative log
/// likelihood of `choices` plus `ridge` times the squared weights, by
/// Newton's method with a backtracking line search. `ridge` must be
/// positive.
pub fn fit(choices: &[Choice], dimension: usize, ridge: f64) -> Vec<f64> {
    let mut weights = vec![0.0; dimension];
    for _ in 0..MAX_NEWTON_STEPS {
        let (value, gradient, hessian) = derivatives(choices, &weights, ridge);
        let descent: Vec<f64> = gradient.iter().map(|g| -g).collect();
        let step = solve_positive_definite(&hessian, &descent);
        // The objective's slope along the step: minus the Newton decrement.
        let slope = dot(&gradient, &step);
        if -slope / 2.0 <= DECREASE_TOLERANCE {
            break;
        }
        // Halve the step until it decreases the objective enough (Armijo's
        // condition); far from the optimum the full step can overshoot.
        let mut scale = 1.0;
        loop {
            let candidate: Vec<f64> = weights
                .iter()
                .zip(&step)
                .map(|(w, s)| w + scale * s)
                .collect();
            if objective(choices, &candidate, ridge) <= value + 1e-4 * scale * slope {
                weights = candidate;
                break;
            }
            scale /= 2.0;
            if scale < 1e-12 {
                // No step decreases it: rounding has the last word.
                return weights;
            }
        }
    }
    weights
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The softmax of `scores`, and the log of its normalizer.
fn probabilities(scores: &[f64]) -> (Vec<f64>, f64) {
    let highest = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exponentials: Vec<f64> = scores.iter().map(|score| (score - highest).exp()).collect();
    let total: f64 = exponentials.iter().sum();
    (
        exponentials.iter().map(|e| e / total).collect(),
        highest + total.ln(),
    )
}

fn scores(choice: &Choice, weights: &[f64]) -> Vec<f64> {
    choice.candidates.iter().map(|x| dot(x, weights)).collect()
}

/// The mean negative log likelihood of the right candidates plus the
/// ridge penalty.
fn objective(choices: &[Choice], weights: &[f64], ridge: f64) -> f64 {
    let likelihood: f64 = choices
        .iter()
        .map(|choice| {
            let scores = scores(choice, weights);
            probabilities(&scores).1 - scores[choice.chosen]
        })
        .sum();
    likelihood / choices.len() as f64 + ridge * dot(weights, weights)
}

/// The [`objective`], its gradient and its Hessian at `weights`.
fn derivatives(choices: &[Choice], weights: &[f64], ridge: f64) -> (f64, Vec<f64>, Vec<Vec<f64>>) {
    let dimension = weights.len();
    let mut value = 0.0;
    let mut gradient = vec![0.0; dimension];
    let mut hessian = vec![vec![0.0; dimension]; dimension];
    for choice in choices {
        let scores = scores(choice, weights);
        let (probabilities, log_normalizer) = probabilities(&scores);
        value += log_normalizer - scores[choice.chosen];
        let mut expected = vec![0.0; dimension];
        for (x, &p) in choice.candidates.iter().zip(&probabilities) {
            for i in 0..dimension {
                expected[i] += p * x[i];
                for j in 0..dimension {
                    hessian[i][j] += p * x[i] * x[j];
                }
            }
        }
        let right = &choice.candidates[choice.chosen];
        for i in 0..dimension {
            gradient[i] += expected[i] - right[i];
            for j in 0..dimension {
                hessian[i][j] -= expected[i] * expected[j];
            }
        }
    }
    let n = choices.len() as f64;
    value = value / n + ridge * dot(weights, weights);
    for i in 0..dimension {
        gradient[i] = gradient[i] / n + 2.0 * ridge * weights[i];
        for entry in &mut hessian[i] {
            *entry /= n;
        }
        hessian[i][i] += 2.0 * ridge;
    }
    (value, gradient, hessian)
}

/// `x` with `matrix · x = rhs`, for a symmetric positive definite
/// `matrix`, by Cholesky factorization. The objective's Hessian is one:
/// the likelihood's is positive semidefinite, and the ridge adds a
/// positive multiple of the identity.
fn solve_positive_definite(matrix: &[Vec<f64>], rhs: &[f64]) -> Vec<f64> {
    let n = rhs.len();
    // matrix = L · Lᵀ, L lower triangular.
    let mut lower = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in 0..=i {
            let dot: f64 = (0..j).map(|k| lower[i][k] * lower[j][k]).sum();
            lower[i][j] = if i == j {
                (matrix[i][i] - dot).sqrt()
            } else {
                (matrix[i][j] - dot) / lower[j][j]
            };
        }
    }
    // L · y = rhs, then Lᵀ · x = y.
    let mut y = vec![0.0; n];
    for i in 0..n {
        let dot: f64 = (0..i).map(|k| lower[i][k] * y[k]).sum();
        y[i] = (rhs[i] - dot) / lower[i][i];
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let dot: f64 = (i + 1..n).map(|k| lower[k][i] * x[k]).sum();
        x[i] = (y[i] - dot) / lower[i][i];
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three candidates per example, the right one carrying the larger
    /// first feature, the second feature noise.
    fn choices() -> Vec<Choice> {
        (0..30)
            .map(|example| {
                let chosen = example % 3;
                let candidates = (0..3)
                    .map(|candidate| {
                        let signal = if candidate == chosen { 1.0 } else { 0.0 };
                        let noise = ((example * 7 + candidate * 3) % 5) as f64 / 5.0;
                        vec![signal + 0.2 * noise, noise]
                    })
                    .collect();
                Choice { candidates, chosen }
            })
            .collect()
    }

    #[test]
    fn the_fit_reaches_the_optimum() {
        let choices = choices();
        let ridge = 1e-3;
        let weights = fit(&choices, 2, ridge);
        let (value, gradient, _) = derivatives(&choices, &weights, ridge);
        assert!(gradient.iter().all(|g| g.abs() <= 1e-8), "{gradient:?}");
        // Any small move away does no better.
        for i in 0..2 {
            for delta in [-1e-3, 1e-3] {
                let mut moved = weights.clone();
                moved[i] += delta;
                assert!(objective(&choices, &moved, ridge) >= value);
            }
        }
        assert_eq!(accuracy(&choices, &weights), 1.0);
        assert!(weights[0] > weights[1].abs());
    }

    #[test]
    fn ties_go_to_the_earliest_candidate() {
        let candidates = vec![vec![1.0], vec![2.0], vec![2.0]];
        assert_eq!(best(&candidates, &[1.0]), 1);
        assert_eq!(best(&candidates, &[0.0]), 0);
    }

    #[test]
    fn the_cholesky_solve_inverts_its_matrix() {
        let n = 24;
        let matrix: Vec<Vec<f64>> = (0..n)
            .map(|i| {
                (0..n)
                    .map(|j| {
                        if i == j {
                            3.0
                        } else {
                            1.0 / (1 + i + j) as f64
                        }
                    })
                    .collect()
            })
            .collect();
        let x: Vec<f64> = (0..n).map(|i| i as f64 - 11.5).collect();
        let rhs: Vec<f64> = matrix.iter().map(|row| dot(row, &x)).collect();
        let solved = solve_positive_definite(&matrix, &rhs);
        for i in 0..n {
            assert!((solved[i] - x[i]).abs() < 1e-9);
        }
    }
}
