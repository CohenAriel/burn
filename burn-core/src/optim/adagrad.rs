use crate::{
    self as burn, grad_clipping::GradientClippingConfig, module::ADModule, record::Record,
    LearningRate,
};

use super::{
    decay::{WeightDecay, WeightDecayConfig, WeightDecayState},
    Optimizer, SimpleOptimizer,
};
use crate::config::Config;
use crate::optim::adaptor::OptimizerAdaptor;
use crate::tensor::{backend::ADBackend, Tensor};
use burn_tensor::backend::Backend;

#[derive(Config)]
pub struct AdaGradConfig {
    #[config(default = 0.)]
    lr_decay: f64,
    #[config(default = 1e-5)]
    epsilon: f32,
    /// [Weight decay](WeightDecayConfig) config.
    weight_decay: Option<WeightDecayConfig>,
    /// [Gradient Clipping](GradientClippingConfig) config.
    grad_clipping: Option<GradientClippingConfig>,
}

pub struct AdaGrad<B: Backend> {
    lr_decay: LRDecay,
    weight_decay: Option<WeightDecay<B>>,
}

#[derive(Record, Clone, new)]
pub struct AdaGradState<B: Backend, const D: usize> {
    weight_decay: Option<WeightDecayState<B, D>>,
    lr_decay: LRDecayState<B, D>,
}

impl<B: Backend> SimpleOptimizer<B> for AdaGrad<B> {
    type State<const D: usize> = AdaGradState<B, D>;

    fn step<const D: usize>(
        &self,
        lr: LearningRate,
        tensor: Tensor<B, D>,
        mut grad: Tensor<B, D>,
        state: Option<Self::State<D>>,
    ) -> (Tensor<B, D>, Option<Self::State<D>>) {
        let mut state_weight_decay = None;
        let mut state_lr_decay = None;

        if let Some(state) = state {
            state_weight_decay = state.weight_decay;
            state_lr_decay = Some(state.lr_decay);
        }

        if let Some(weight_decay) = &self.weight_decay {
            let (grad_out, state) = weight_decay.transform(grad, state_weight_decay);
            state_weight_decay = Some(state);
            grad = grad_out;
        }

        let (grad, state_lr_decay) = self.lr_decay.transform(grad, lr, state_lr_decay);

        let state = AdaGradState::new(state_weight_decay, state_lr_decay);

        (tensor - grad, Some(state))
    }

    fn to_device<const D: usize>(
        mut state: Self::State<D>,
        device: &<B as Backend>::Device,
    ) -> Self::State<D> {
        state.weight_decay = state.weight_decay.map(|state| state.to_device(device));
        state.lr_decay = state.lr_decay.to_device(device);
        state
    }
}

impl AdaGradConfig {
    pub fn init<B: ADBackend, M: ADModule<B>>(&self) -> impl Optimizer<M, B> {
        let optim = AdaGrad {
            lr_decay: LRDecay {
                lr_decay: self.lr_decay,
                epsilon: self.epsilon,
            },
            weight_decay: self.weight_decay.as_ref().map(WeightDecay::new),
        };

        let mut optim = OptimizerAdaptor::from(optim);
        if let Some(config) = &self.grad_clipping {
            optim = optim.with_grad_clipping(config.init());
        }
        optim
    }
}

#[derive(Record, new, Clone)]
pub struct LRDecayState<B: Backend, const D: usize> {
    time: usize,
    sum: Tensor<B, D>,
}

struct LRDecay {
    lr_decay: f64,
    epsilon: f32,
}

impl LRDecay {
    pub fn transform<B: Backend, const D: usize>(
        &self,
        grad: Tensor<B, D>,
        lr: LearningRate,
        lr_decay_state: Option<LRDecayState<B, D>>,
    ) -> (Tensor<B, D>, LRDecayState<B, D>) {
        let state = if let Some(mut state) = lr_decay_state {
            state.sum = state.sum.add(grad.clone().powf(2.));
            state.time += 1;
            state
        } else {
            LRDecayState::new(1, grad.clone().powf(2.))
        };

        let new_lr = lr / (1. + (state.time as f64 - 1.) * self.lr_decay);

        let grad = grad
            .clone()
            .div(state.sum.clone().sqrt().add_scalar(self.epsilon))
            .mul_scalar(new_lr);

        (grad, state)
    }
}

impl<B: Backend, const D: usize> LRDecayState<B, D> {
    /// Move state to device.
    ///
    /// # Arguments
    ///
    /// * `device` - Device to move state to.
    ///
    /// # Returns
    ///
    /// Returns state moved to device.
    pub fn to_device(mut self, device: &B::Device) -> Self {
        self.sum = self.sum.to_device(device);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::{Module, Param};
    use crate::optim::{GradientsParams, Optimizer};
    use crate::record::{BinFileRecorder, FullPrecisionSettings, Recorder};
    use crate::tensor::{Data, Distribution, Tensor};
    use crate::{nn, TestADBackend, TestBackend};

    const LEARNING_RATE: LearningRate = 0.01;

    #[test]
    fn test_adagrad_optimizer_save_load_state() {
        let linear = nn::LinearConfig::new(6, 6).init();
        let x = Tensor::<TestADBackend, 2>::random([2, 6], Distribution::Default);
        let mut optimizer = create_adagrad();
        let grads = linear.forward(x).backward();
        let grads = GradientsParams::from_grads(grads, &linear);
        let _linear = optimizer.step(LEARNING_RATE, linear, grads);
        BinFileRecorder::<FullPrecisionSettings>::default()
            .record(optimizer.to_record(), "/tmp/test_optim".into())
            .unwrap();

        let state_optim_before = optimizer.to_record();
        let state_optim_before_copy = optimizer.to_record();
        let optimizer = create_adagrad();
        let optimizer = optimizer.load_record(state_optim_before_copy);
        let state_optim_after = optimizer.to_record();

        assert_eq!(state_optim_before.len(), state_optim_after.len());
    }
    const ASSERT_PRECISION: usize = 6;

    #[test]
    fn test_adagrad_optimizer_with_numbers() {
        let linear = given_linear_layer(
            Data::from([
                [-0.3206, 0.1374, 0.4043, 0.3200, 0.0859, 0.0671],
                [0.0777, -0.0185, -0.3667, 0.2550, 0.1955, -0.2922],
                [-0.0190, 0.0346, -0.2962, 0.2484, -0.2780, 0.3130],
                [-0.2980, -0.2214, -0.3715, -0.2981, -0.0761, 0.1626],
                [0.3300, -0.2182, 0.3717, -0.1729, 0.3796, -0.0304],
                [-0.0159, -0.0120, 0.1258, 0.1921, 0.0293, 0.3833],
            ]),
            Data::from([-0.3905, 0.0884, -0.0970, 0.1176, 0.1366, 0.0130]),
        );
        let x_1 = Tensor::from_floats([
            [0.6294, 0.0940, 0.8176, 0.8824, 0.5228, 0.4310],
            [0.7152, 0.9559, 0.7893, 0.5684, 0.5939, 0.8883],
        ])
        .require_grad();
        let x_2 = Tensor::from_floats([
            [0.8491, 0.2108, 0.8939, 0.4433, 0.5527, 0.2528],
            [0.3270, 0.0412, 0.5538, 0.9605, 0.3195, 0.9085],
        ])
        .require_grad();

        let mut optimizer = AdaGradConfig::new()
            .with_epsilon(1e-8)
            .with_lr_decay(0.5)
            .init();

        let grads = linear.forward(x_1).backward();
        let grads = GradientsParams::from_grads(grads, &linear);
        let linear = optimizer.step(LEARNING_RATE, linear, grads);

        let grads = linear.forward(x_2).backward();
        let grads = GradientsParams::from_grads(grads, &linear);
        let linear = optimizer.step(LEARNING_RATE, linear, grads);

        let state_updated = linear.into_record();
        let weights_expected = Data::from([
            [-0.334989, 0.123011, 0.389911, 0.305611, 0.071511, 0.052711],
            [
                0.066144, -0.030056, -0.378256, 0.243444, 0.183944, -0.303756,
            ],
            [
                -0.033462, 0.020138, -0.310662, 0.233938, -0.292462, 0.298538,
            ],
            [
                -0.312636, -0.236036, -0.386136, -0.312736, -0.090736, 0.147964,
            ],
            [
                0.315896, -0.232304, 0.357596, -0.187004, 0.365496, -0.044504,
            ],
            [-0.030305, -0.026405, 0.111395, 0.177695, 0.014895, 0.368895],
        ]);
        let bias_expected = Data::from([
            -0.405214, 0.073686, -0.111714, 0.102886, 0.121886, -0.001714,
        ]);

        let (weight_updated, bias_updated) = (
            state_updated.weight.to_data(),
            state_updated.bias.unwrap().to_data(),
        );

        bias_updated.assert_approx_eq(&bias_expected, ASSERT_PRECISION);
        weight_updated.assert_approx_eq(&weights_expected, ASSERT_PRECISION);
    }

    fn given_linear_layer(weight: Data<f32, 2>, bias: Data<f32, 1>) -> nn::Linear<TestADBackend> {
        let record = nn::LinearRecord {
            weight: Param::from(Tensor::from_data(weight)),
            bias: Some(Param::from(Tensor::from_data(bias))),
        };

        nn::LinearConfig::new(6, 6).init_with(record)
    }

    fn create_adagrad(
    ) -> OptimizerAdaptor<AdaGrad<TestBackend>, nn::Linear<TestADBackend>, TestADBackend> {
        let config = AdaGradConfig::new();
        AdaGrad {
            lr_decay: LRDecay {
                lr_decay: config.lr_decay,
                epsilon: config.epsilon,
            },
            weight_decay: config.weight_decay.as_ref().map(WeightDecay::new),
        }
        .into()
    }
}